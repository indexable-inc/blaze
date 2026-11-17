//! One `CrateScope` task, in three steps, so a crate's names are resolved on a pool worker
//! (drive.md, RUNNER 17): [`prepare`] on the main thread, [`resolve`] on any worker, [`settle`]
//! and [`answer`] on the main thread again.
//!
//! Why it ran on the main thread before: the task borrowed the command's live names state.
//! [`Names`]' world reads the checkout through a reader that allocates into one frame through
//! the main thread's context (`&dyn Tree`, not `Sync`); the store is `&mut dyn Store`; the
//! command's one `Prepares` (compiled macro definitions) holds an `Alloc` handle (not `Sync`);
//! the crate's `CrateEnv` likewise; and every resolution allocated in the global heap, never
//! freed. Now a job shares RECORD BYTES and read-only views only: its dependencies' exports
//! records (read from the store on the main thread, once a round each), the packs' views and
//! the corrections (copied once a round), the crate's environment lent to be copied
//! ([`EnvLent`]). The worker reads the checkout files the crate names itself
//! ([`JobTexts`]), compiles the macros it calls and holds everything it makes in its own task
//! frame, freed when the task ends; what it returns is the crate's record, owned.
//!
//! A crate's record is a function of its job: the same bytes whichever worker, order or batch
//! made it (rule 25; the test `a_pooled_check_writes_the_records_a_serial_one_does`, main.rs).

use core::cell::RefCell;
use std::alloc::Global;
use std::string::String;

use blaze_build_script::outputs::OutputsRef;
use blaze_drive::{Input, Key, Kind, Locate, Name, Part, Task, TaskFailure, Want, Why};
use blaze_fetch::pack::PackView;
use blaze_id::CrateItemKey;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{Alloc, Erased};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::crate_scope::{CrateScopeError, resolve_crate};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{CrateInput, SourceTexts};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Frame, Tag};

use super::{Names, OUT_DOMAIN, RULE, Resolved, UNRESOLVED, name_of, scope_record, sections, slot_of, surface, tell_public, world};
use crate::CliError;
use crate::fetch::FetchEnv;
use crate::job_texts::JobTexts;
use crate::store::Store;
use crate::work::{Did, Made, held_str};

/// The most one crate's resolution holds in its task frame: address space reserved, committed
/// as used (the largest crate of blaze's clone resolves in far less). Past it the task fails
/// alone, naming its frame (rule 20).
pub(crate) const SCOPE_FRAME: usize = 4 << 30;

/// The most a round's crate scope jobs hold together (their dependencies' exports records,
/// each once a round): reserved, committed as used, reset when the round's scope has joined.
pub(crate) const SCOPE_JOBS: usize = 4 << 30;

/// One crate to resolve, as a pool worker is handed it: all of it read-only and `Sync`.
pub(crate) struct ScopeJob<'j> {
    /// The crate's index in the world (its texts' `krate`).
    at: usize,
    /// Its input, its dependencies renumbered to `deps`' order.
    input: CrateInput<'j>,
    /// The exports record of each crate it reads, 16-byte aligned, in its local order.
    deps: &'j [&'j [u8]],
    sysroot: Sysroot,
    packs: &'j [PackView<'j>],
    corrected: &'j [(u32, &'j str, &'j str)],
    env: Option<EnvLent<'j, 'j>>,
    /// The command's proc-macro runner (`Names::procs`).
    procs: Option<&'j crate::proc_runner::ProcRunner<'static>>,
}

/// What a round's jobs share, made once a round in its frame: the packs' views and the
/// corrections, and each exports record read (by its key), so ten crates that read `core` read
/// its record once (rule 77).
pub(crate) struct ScopeRound<'j> {
    frame: &'j Frame,
    alloc: Alloc<'j>,
    texts: Option<(&'j [PackView<'j>], &'j [(u32, &'j str, &'j str)])>,
    /// Each exports record read this round: its key, the place it had in the list of the crate
    /// that read it first, its aligned bytes.
    exports: blaze_own::Vec<(Key, usize, &'j [u8]), &'j Frame>,
    /// The tests' planted bug (`work::Plant::ScopeShareByPlace`): look records up by place.
    by_place: bool,
}

impl<'j> ScopeRound<'j> {
    pub(crate) fn new(frame: &'j Frame, by_place: bool) -> ScopeRound<'j> {
        let erased: &'j Erased<&'j Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(Erased::new(frame), frame));
        ScopeRound { frame, alloc: erased.handle(), texts: None, exports: blaze_own::Vec::new_in(frame), by_place }
    }

    /// The round's frame, held until the round's scope has joined.
    pub(crate) fn frame(&self) -> &'j Frame {
        self.frame
    }
}

/// A `CrateScope` task's first step.
pub(crate) enum Prepared<'b, 'j> {
    /// Its answer without a resolution (a need, a failure).
    Answer(Did<'b>),
    /// Its crate was resolved this command: [`answer`] files it.
    Made,
    /// Its crate is to be resolved now.
    Resolve(&'j ScopeJob<'j>),
}

/// A crate's resolution as its job made it, owned: what [`settle`] files on the main thread.
pub(crate) enum Resolution {
    /// Resolved (or failed, its failure filed in `resolved`); `ok`: the resolution itself
    /// succeeded; `unresolved`: its unresolved paths.
    Made { resolved: Resolved, ok: bool, unresolved: u32 },
    /// It needs these crates' records first.
    Need(std::vec::Vec<CrateItemKey, Global>),
}

/// The crates a crate reads, by world index, in its local order: direct dependencies, the
/// sysroot crates, then each a `Need` named (any `CrateScope` input beyond those).
struct Locals<'f> {
    local: std::vec::Vec<usize, Global>,
    own_deps: std::vec::Vec<(&'f str, usize), Global>,
    mapped: [Option<usize>; 4],
    all: [Option<CrateId>; 4],
}

/// Which of the sysroot crates `all` (core, alloc, std, bstd, by world index) the crate at
/// `at`, whose own dependencies are `own`, is given. A toolchain crate (`toolchain`): those its
/// shelf row names, which are its edges (the cutter writes the sysroot crates a root names as
/// real dependencies: blaze_toolchain::implied), read here and nowhere else. A crate of the
/// program: every one the world holds (which `std` it names is its `Implicit`'s; what its
/// root's `#![no_std]` drops is blaze-scope graph.rs `externs`'). Never the crate itself.
fn sysroot_given(toolchain: bool, at: usize, all: [Option<usize>; 4], own: &[(&str, usize)]) -> [bool; 4] {
    let mut given = [false; 4];
    for (slot, s) in given.iter_mut().zip(all) {
        let Some(s) = s else { continue };
        *slot = s != at && (!toolchain || own.iter().any(|(_, d)| *d == s));
    }
    given
}

fn locals<'f>(names: &Names<'f>, at: usize, task: &Task<'_>) -> Locals<'f> {
    let crates = &names.world.crates;
    let input = crates[at].input;
    let mut local: std::vec::Vec<usize, Global> = std::vec::Vec::new();
    let place = |global: usize, local: &mut std::vec::Vec<usize, Global>| match local.iter().position(|g| *g == global) {
        Some(p) => p,
        None => {
            local.push(global);
            local.len() - 1
        }
    };
    let mut own_deps: std::vec::Vec<(&'f str, usize), Global> = std::vec::Vec::with_capacity(input.deps.len());
    for &(dep, index) in input.deps {
        own_deps.push((dep, place(index, &mut local)));
    }
    let sysroot = names.world.sysroot;
    let all = [sysroot.core, sysroot.alloc, sysroot.std, sysroot.bstd];
    // A toolchain crate is given the sysroot crates its shelf row names (its edges: the
    // cutter's implied `core`, `alloc`, `std`), so bstd and the blaze crates under it, none of
    // which lists `core` in a manifest, are given it, and std's own closure is never given
    // std; a program crate every one the world holds (test
    // `a_toolchain_crate_is_given_what_its_row_names`).
    let toolchain = matches!(crates[at].origin, blaze_world::Origin::Toolchain(_));
    let given = sysroot_given(toolchain, at, all.map(|s| s.map(|s| s.index())), input.deps);
    let mut mapped = [None; 4];
    for ((slot, s), given) in mapped.iter_mut().zip(all).zip(given) {
        *slot = s.filter(|_| given).map(|s| place(s.index(), &mut local));
    }
    for input in task.inputs.iter().filter(|input| input.name.kind == Kind::CrateScope) {
        if let Ok(i) = names.by_subject.binary_search_by_key(&input.name.subject, |(subject, _)| *subject) {
            place(names.by_subject[i].1, &mut local);
        }
    }
    Locals { local, own_deps, mapped, all }
}

/// The input of the crate at world index `g`, if the task was handed it. The name is hashed
/// once per lookup, never per input compared (as body_types.rs `input_at`).
fn have<'t>(names: &Names<'_>, task: &'t Task<'_>, g: usize) -> Option<&'t Input> {
    let name = name_of(names.world.crates[g].input.key);
    task.inputs.iter().find(|input| input.name == name)
}

/// The task's first step, on the main thread: the world made if this command has none yet, the
/// crate found, its needs checked; a crate not resolved this command gets its job, made in
/// `round` (its dependencies' exports records read from `store`).
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn prepare<'f: 'j, 'j, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    names: &mut Option<Names<'f>>,
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
    env: &FetchEnv<'_>,
    macros: Option<&crate::proc_runner::MacroSide<'_, '_, '_>>,
    task: &Task<'_>,
    round: &mut ScopeRound<'j>,
    held: &'b Frame,
) -> Result<Prepared<'b, 'j>, CliError> {
    let cx = cx.span("crate scope: what one crate reads");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    if names.is_none() {
        let Some(graph) = task.inputs.iter().find(|input: &&Input| input.name == Name::unit_graph()) else {
            let mut wants = blaze_own::Vec::with_capacity_in(1, held);
            wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
            return Ok(Prepared::Answer(Did::Need(wants)));
        };
        let read = crate::work::payload(&cx, store, graph, round.frame)?;
        match world(&cx, root, read.unit_graph(graph.name)?.graph, lock, members, env) {
            Ok(made) => {
                tell_public(store, &made);
                *names = Some(made);
            }
            Err(error) => {
                let words = bstd::mem::format_in!(held, "names not resolved: {error}");
                return Ok(Prepared::Answer(Did::FailedSaying(refused("the crates' inputs could not be made"), held_str(words.as_str(), held))));
            }
        }
    }
    // The proc-macro runner, once a command, right after the world: every crate's job carries
    // it, so a proc-macro crate's record holds its key and every call reaches the one path.
    if let (Some(made), Some(side)) = (names.as_mut(), macros)
        && made.procs().is_none()
    {
        match crate::proc_runner::make(made, side) {
            Ok(runner) => made.set_procs(runner),
            Err(error) => {
                let words = bstd::mem::format_in!(held, "names not resolved: the proc-macro runner: {error}");
                return Ok(Prepared::Answer(Did::FailedSaying(refused("the proc-macro runner could not be made"), held_str(words.as_str(), held))));
            }
        }
    }
    let Some(names) = names.as_ref() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
    let Ok(at) = names.by_subject.binary_search_by_key(&name.subject, |(subject, _)| *subject).map(|i| names.by_subject[i].1) else {
        return Ok(Prepared::Answer(Did::FailedSaying(refused("a crate the world does not hold"), "a `CrateScope` part names a crate this command's world does not hold")));
    };
    // A crate the list refused (a member proc macro the toolchain shelf has no wasm32-wasip1
    // side for) is never resolved with no dependencies (rule 3: its every path into `syn` or
    // `quote` would be NotFound at segment 0): it fails alone, naming itself and the fix.
    // `World::refused` is sorted by `krate`, each once.
    if let Ok(i) = names.world.refused.binary_search_by_key(&at, |r| r.krate) {
        let why = &names.world.refused[i].why;
        let words = bstd::mem::format_in!(held, "crate `{}`: {why}", names.world.crates[at].input.name);
        return Ok(Prepared::Answer(Did::FailedSaying(refused("a crate the toolchain shelf cannot build"), held_str(words.as_str(), held))));
    }
    // Any need that failed this command (a dependency's names, a file's surface whose outline
    // failed): this crate fails `Waited` on it, alone (rule 20); asking again would loop.
    if let Some(failed) = task.failed.first() {
        return Ok(Prepared::Answer(Did::Failed(TaskFailure { name, why: Why::Waited { on: *failed } })));
    }
    let crates = &names.world.crates;
    let Locals { local, own_deps, mapped, all } = locals(names, at, task);
    let missing: std::vec::Vec<usize, Global> = local.iter().copied().filter(|g| have(names, task, *g).is_none()).collect();
    // Its unit's build script's run (decision D115 (B)): read now, never in the crate's key.
    let script = names.script_of(at);
    let script_handed = script.and_then(|script| task.inputs.iter().find(|input| input.name == script));
    if !missing.is_empty() || (script.is_some() && script_handed.is_none()) {
        let mut wants = blaze_own::Vec::with_capacity_in(missing.len() + 1, held);
        wants.extend(missing.iter().map(|g| Want { name: name_of(crates[*g].input.key), home: Name::unit_graph() }));
        wants.extend(script.filter(|_| script_handed.is_none()).map(|script| Want { name: script, home: Name::unit_graph() }));
        return Ok(Prepared::Answer(Did::Need(wants)));
    }
    if names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject).is_ok() {
        return Ok(Prepared::Made);
    }
    // The crate's input with its script's outputs joined (`rustc-cfg` atoms, the record for
    // `env!`/`include!`), in the round's frame.
    let (input, outputs): (CrateInput<'j>, Option<OutputsRef<'j>>) = match script_handed {
        None => (crates[at].input, None),
        Some(handed) => {
            let read = crate::work::payload(&cx, store, handed, round.frame)?;
            let bytes: &'j [u8] = blaze_own::Vec::from_slice_in(read.payload(), round.frame).leak();
            // A `Script` payload: how it ran, then the outputs record (scripts.rs `payload_parts`).
            // Its stored files (a native script's `OUT_DIR` files, in the home's file area) are
            // read from there by an `include!` that names one, each mapped on its first ask:
            // nothing of them is read here.
            let opened = match crate::scripts::payload_parts(bytes) {
                Some((_, record)) => OutputsRef::open(record).map(|outputs| {
                    let area: &'j blaze_world::StoredFiles = blaze_own::Box::leak(blaze_own::Box::new_in(crate::scripts::file_area(env.home, &outputs), round.frame));
                    outputs.with_area(area)
                }),
                None => Err(blaze_build_script::outputs::RecordError::Layout),
            };
            match opened {
                Ok(outputs) => (blaze_world::joined(&crates[at].input, outputs, round.alloc), Some(outputs)),
                Err(error) => {
                    let words = bstd::mem::format_in!(held, "crate `{}`: its build script's record ({:?}) is not an outputs record: {error:?}", crates[at].input.name, handed.name);
                    return Ok(Prepared::Answer(Did::FailedSaying(refused("a build script's record"), held_str(words.as_str(), held))));
                }
            }
        }
    };
    let mut deps: blaze_own::Vec<&'j [u8], &'j Frame> = blaze_own::Vec::with_capacity_in(local.len(), round.frame);
    for (place, g) in local.iter().enumerate() {
        let handed = have(names, task, *g).ok_or(CliError::Text { what: "a crate's input, checked above, is gone" })?;
        let by_place = round.by_place;
        if let Some((_, _, bytes)) = round.exports.iter().find(|(key, at, _)| if by_place { *at == place } else { *key == handed.at }) {
            deps.push(bytes);
            continue;
        }
        let read = crate::work::payload(&cx, store, handed, round.frame)?;
        let record = match sections::sections(read.payload()) {
            Ok(parts) => parts.exports,
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: the record of its dependency `{}`: {error}", input.name, crates[*g].input.name);
                return Ok(Prepared::Answer(Did::FailedSaying(refused("a dependency's record"), held_str(words.as_str(), held))));
            }
        };
        #[cfg(test)]
        sections::watch(crates[*g].input.name, read.payload());
        let bytes = blaze_scope::resolve::aligned(record, round.alloc);
        round.exports.push((handed.at, place, bytes));
        deps.push(bytes);
    }
    // Every crate the dependencies' records reach and none of them is, read from the records'
    // heads in place: asked for here, all at once, so the pooled resolve never decodes every
    // dependency only to answer `Need` (rule 77; tempo's cold check made 2,668 pooled
    // resolutions for 1,550 records, the rest such answers).
    match blaze_scope::crate_scope::unmet_reaches(&deps, round.alloc) {
        Ok(unmet) if unmet.is_empty() => {}
        Ok(unmet) => return Ok(Prepared::Answer(need_crates(names, name, &unmet, held))),
        Err(error) => {
            let words = bstd::mem::format_in!(held, "crate `{}`: a dependency's record: {error:?}", input.name);
            return Ok(Prepared::Answer(Did::FailedSaying(refused("a dependency's record"), held_str(words.as_str(), held))));
        }
    }
    let me = local.len();
    let own = |s: Option<CrateId>| s.filter(|s| s.index() == at).map(|_| CrateId(me as u32));
    let ids = mapped.map(|p| p.map(|p| CrateId(p as u32)));
    let sysroot = Sysroot { core: ids[0].or(own(all[0])), alloc: ids[1].or(own(all[1])), std: ids[2].or(own(all[2])), bstd: ids[3].or(own(all[3])) };
    let own_deps: &'j [(&'f str, usize)] = blaze_own::Vec::from_slice_in(&own_deps, round.frame).leak();
    let (packs, corrected) = match round.texts {
        Some(texts) => texts,
        None => {
            let packs: &'j [PackView<'j>] = blaze_own::Vec::from_slice_in(names.world.texts.packs(), round.frame).leak();
            let corrected: &'j [(u32, &'j str, &'j str)] = blaze_own::Vec::from_slice_in(names.world.texts.corrected(), round.frame).leak();
            round.texts = Some((packs, corrected));
            (packs, corrected)
        }
    };
    // The environment is copied here into the round's frame, so the job outlives this borrow
    // of the world; the worker copies it again into its own frame ([`EnvLent`]). With a script,
    // its `OUT_DIR` and `rustc-env` pairs joined (`CrateEnv::joined`, a copy too).
    let env = match (names.world.texts.crate_env(at), outputs) {
        (None, _) => None,
        (Some(env), Some(outputs)) => Some(EnvLent::new(&*blaze_own::Box::leak(blaze_own::Box::new_in(env.joined(outputs, round.alloc), round.alloc)))),
        (Some(env), None) => match env.clone_in(round.alloc) {
            Ok(copy) => Some(EnvLent::new(&*blaze_own::Box::leak(blaze_own::Box::new_in(copy, round.alloc)))),
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: its environment's build-script record did not open again: {error:?}", input.name);
                return Ok(Prepared::Answer(Did::FailedSaying(refused("a crate's environment"), held_str(words.as_str(), held))));
            }
        },
    };
    let job = ScopeJob { at, input: CrateInput { deps: own_deps, ..input }, deps: deps.leak(), sysroot, packs, corrected, env, procs: names.procs() };
    Ok(Prepared::Resolve(blaze_own::Box::leak(blaze_own::Box::new_in(job, round.frame))))
}

/// The crate resolved, on whichever thread runs it, in `frame` (a pool task's own): its record
/// and read lists, owned, and what its frame held by the stretch of the solve that took it
/// ([`super::metered`]). Reads nothing but `job` and the checkout files the crate names.
pub(crate) fn resolve(cx: &Cx, root: &Dir<ReadWrite>, job: &ScopeJob<'_>, frame: &Frame) -> (Resolution, super::metered::ScopeBytes) {
    let metered = super::metered::Metered::new(frame);
    let resolution = resolve_metered(cx, root, job, frame, &metered);
    let of = crate::phases::byte_rows::Label::of(format_args!("{}", job.input.name));
    (resolution, metered.finish(of))
}

fn resolve_metered(cx: &Cx, root: &Dir<ReadWrite>, job: &ScopeJob<'_>, frame: &Frame, metered: &super::metered::Metered<'_>) -> Resolution {
    let cx = cx.span("crate scope: resolve one crate's names");
    let erased = Erased::new(frame);
    let alloc = erased.handle();
    let crate_name = job.input.name;
    let failed_with = |words: String| Resolution::Made {
        resolved: Resolved {
            out: crate::work::failed_out(),
            payload: sections::write(0, [&[][..]; sections::SECTIONS]),
            files: std::vec::Vec::new(),
            absent: std::vec::Vec::new(),
            data: std::vec::Vec::new(),
            findings: blaze_own::Vec::new_in(Global),
            left_package: blaze_own::Vec::new_in(Global),
            failed: Some(words),
            failed_calls: blaze_own::Vec::new_in(Global),
        },
        ok: false,
        unresolved: 0,
    };
    let env = match job.env {
        None => None,
        Some(lent) => match lent.copy_in(alloc) {
            Ok(env) => Some(&*blaze_own::Box::leak(blaze_own::Box::new_in(env, alloc))),
            Err(error) => return failed_with(std::format!("crate `{crate_name}`: its environment's build-script record did not open again: {error:?}")),
        },
    };
    let callers: &[(usize, usize, blaze_expand::Edition)] = blaze_own::Box::leak(blaze_own::Box::new_in([(job.at, job.at, job.input.edition)], frame));
    let texts = JobTexts {
        cx: &cx,
        root,
        frame,
        packs: job.packs,
        corrected: job.corrected,
        env: env.map(|env| (job.at, env)),
        read: RefCell::new(std::vec::Vec::new()),
        data: RefCell::new(std::vec::Vec::new()),
        left: RefCell::new(std::vec::Vec::new()),
        procs: job.procs,
        own: Some(job.at),
        callers,
    };
    let prepares = Prepares::new_in(alloc);
    let done = {
        let _cx = cx.span("crate scope: resolve_crate");
        resolve_crate(&job.input, job.deps, job.sysroot, &texts, job.at, metered, prepares, alloc)
    };
    metered.outside(super::metered::Outside::Record);
    // Each include that left its package inside the workspace (D111): a warning finding at
    // its call, placed in the crate's own files (none when the crate failed).
    let left_package = match &done {
        Ok(done) => crate::names::include_outside::findings(done.names, &texts.left.borrow(), job.input.shown_from),
        Err(_) => blaze_own::Vec::new_in(Global),
    };
    // The out of a resolved crate: its names record here, its files below ([`OUT_DOMAIN`]);
    // `None`: failed ([`crate::work::failed_out`]).
    // Each failed macro call of the crate's own code (a `main` the implicit `::std::main` did
    // not write among them): an error finding at the call with the matcher's reason.
    let failed_calls = match &done {
        Ok(done) => crate::names::failed_call::findings(done.names),
        Err(_) => blaze_own::Vec::new_in(Global),
    };
    let (out, payload, findings, failed, ok, unresolved) = match done {
        Ok(done) => match scope_record(&done, crate_name, alloc) {
            Ok(record) => {
                // Both records (rule 21): a dependent reads the exports, but the crate's own
                // bodies read its inner names (front_load.rs), so an out over the exports alone
                // left a private item's removal moving no reader: its body was handed the record
                // that still resolved the item gone (`SourceError::ItemStale`, lane UN
                // 2026-11-17; signature_grain_tests `a_name_that_does_not_exist_is_an_error`).
                let mut out = blaze_ids::Hasher::new(OUT_DOMAIN);
                out.update(&(done.record.len() as u64).to_le_bytes()).update(&done.record);
                out.update(&(done.inner.len() as u64).to_le_bytes()).update(&done.inner);
                (Some(out), record.payload, record.findings, record.tests_refused, true, record.unresolved)
            }
            Err(unwritten) => (None, sections::write(0, [&[][..]; sections::SECTIONS]), blaze_own::Vec::new_in(Global), Some(std::format!("{unwritten}")), true, 0),
        },
        Err(CrateScopeError::Need(keys)) => return Resolution::Need(keys.iter().copied().collect()),
        // The crate's typed failure is its answer (work.rs, `filed`): a record whose out
        // says failed, with the files it read so far as its read list, so it stands until
        // one of them moves and a reader of it fails `Waited` on it.
        Err(error) => (
            None,
            sections::write(0, [&[][..]; sections::SECTIONS]),
            blaze_own::Vec::new_in(Global),
            Some(std::format!("crate `{crate_name}`: its names could not be resolved: {error:?}")),
            false,
            0,
        ),
    };
    // The drive names a module file by its slot (`x.rs` for `x/mod.rs` too, sweep.rs): a
    // read of either place is a read of the slot's `Surface`.
    let (mut read, mut lacked): (std::vec::Vec<String, Global>, std::vec::Vec<String, Global>) = (std::vec::Vec::new(), std::vec::Vec::new());
    for (path, text) in texts.read.into_inner() {
        match text {
            Some(_) => read.push(String::from(path)),
            None => lacked.push(String::from(path)),
        }
    }
    read.sort_unstable();
    lacked.sort_unstable();
    let (mut files, mut absent) = (read.clone(), lacked.clone());
    let looked = |slot: &str| [&read, &lacked].iter().any(|list| list.binary_search_by(|path| path.as_str().cmp(slot)).is_ok());
    for path in files.iter_mut().chain(absent.iter_mut()) {
        *path = slot_of(core::mem::take(path), looked);
    }
    files.sort_unstable();
    files.dedup();
    absent.sort_unstable();
    absent.dedup();
    absent.retain(|path| files.binary_search(path).is_err());
    // Each file read as data, with the slot of the file whose call read it: that file's
    // outline lists it as a data part, so it is the home its want names.
    let mut data: std::vec::Vec<(String, String), Global> =
        texts.data.into_inner().into_iter().map(|(path, from)| (String::from(path), slot_of(String::from(from), looked))).collect();
    data.sort_unstable();
    data.dedup();
    // The checkout files the crate holds, each at its own place (`x/mod.rs`, not its slot:
    // a body's file is matched by its path), sorted: what a body's check reads of the
    // items record to find its owner.
    let out = match out {
        Some(mut hasher) => {
            hasher.update(&(read.len() as u64).to_le_bytes());
            for path in read.iter() {
                hasher.update(&(path.len() as u64).to_le_bytes()).update(path.as_bytes());
            }
            hasher.finish()
        }
        None => crate::work::failed_out(),
    };
    Resolution::Made { resolved: Resolved { out, payload, files, absent, data, findings, left_package, failed, failed_calls }, ok, unresolved }
}

/// A resolution filed on the main thread: a need is the task's answer; a made crate is kept
/// for [`answer`] (the first kept stands: two equal jobs make equal bytes).
/// The records of the crates `keys` asked for, each by its `CrateScope` name.
fn need_crates<'b>(names: &Names<'_>, name: Name, keys: &[CrateItemKey], held: &'b Frame) -> Did<'b> {
    let mut wants = blaze_own::Vec::with_capacity_in(keys.len(), held);
    for key in keys.iter() {
        let Ok(i) = names.by_key.binary_search_by_key(key, |(k, _)| *k) else {
            let failure = TaskFailure { name, why: Why::Refused { what: "a needed crate the world does not hold", rule: RULE } };
            return Did::FailedSaying(failure, "the resolution needs a crate's record this command's world does not hold");
        };
        wants.push(Want { name: name_of(names.world.crates[names.by_key[i].1].input.key), home: Name::unit_graph() });
    }
    Did::Need(wants)
}

pub(crate) fn settle<'b>(names: &mut Names<'_>, task: &Task<'_>, resolution: Resolution, held: &'b Frame) -> Option<Did<'b>> {
    let name = task.name;
    match resolution {
        Resolution::Need(keys) => Some(need_crates(names, name, &keys, held)),
        Resolution::Made { resolved, ok, unresolved } => {
            if ok {
                names.resolved += 1;
            }
            names.unresolved += u64::from(unresolved);
            if let Err(i) = names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject) {
                names.made.insert(i, (name.subject, resolved));
            }
            None
        }
    }
}

/// The last step, for a crate made this command: each checkout file it read is asked for as
/// its `Surface` (with the dependencies' records again, so the entry after holds them all);
/// with all of them handed in, its record, its payload moved out of `names` ([`hand_payload`]).
pub(crate) fn answer<'b>(names: &mut Names<'_>, task: &Task<'_>, held: &'b Frame) -> Result<Did<'b>, CliError> {
    let name = task.name;
    let Ok(at) = names.by_subject.binary_search_by_key(&name.subject, |(subject, _)| *subject).map(|i| names.by_subject[i].1) else {
        return Err(CliError::Text { what: "a made crate the world does not hold" });
    };
    let Ok(i) = names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject) else {
        return Err(CliError::Text { what: "a crate scope answered before its crate was made" });
    };
    let resolved = &names.made[i].1;
    let local = locals(names, at, task).local;
    let crates = &names.world.crates;
    let unread: std::vec::Vec<&String, Global> = resolved.files.iter().filter(|path| !task.inputs.iter().any(|input| input.name == surface(path))).collect();
    // Each file read as data is read as its own `FileItems` record (its bytes' id), listed by
    // the outline of the file whose call read it (data_include.rs): never a `Surface`.
    let unread_data: std::vec::Vec<&(String, String), Global> =
        resolved.data.iter().filter(|(path, _)| !task.inputs.iter().any(|input| input.name == blaze_drive::Name::file(path.as_bytes()))).collect();
    let script = names.script_of(at);
    if !unread.is_empty() || !unread_data.is_empty() {
        let mut wants = blaze_own::Vec::with_capacity_in(unread.len() + unread_data.len() + local.len() + 1, held);
        wants.extend(unread.iter().map(|path| Want { name: surface(path), home: blaze_drive::Name::file(path.as_bytes()) }));
        wants.extend(unread_data.iter().map(|(path, home)| Want { name: blaze_drive::Name::file(path.as_bytes()), home: blaze_drive::Name::file(home.as_bytes()) }));
        wants.extend(local.iter().map(|g| Want { name: name_of(crates[*g].input.key), home: Name::unit_graph() }));
        wants.extend(script.map(|script| Want { name: script, home: Name::unit_graph() }));
        return Ok(Did::Need(wants));
    }
    let mut made = Made::new_in(name, held);
    made.out = resolved.out;
    for finding in resolved.findings.iter() {
        let words = core::str::from_utf8(&finding.message).map_err(|_| CliError::Text { what: "a path's segment is not UTF-8" })?;
        made.find(UNRESOLVED, finding.at, &[words]);
    }
    for finding in resolved.failed_calls.iter() {
        let words = core::str::from_utf8(&finding.message).map_err(|_| CliError::Text { what: "a failed call's words are not UTF-8" })?;
        made.find(crate::names::failed_call::FAILED_CALL, finding.at, &[words]);
    }
    if let Some(words) = &resolved.failed {
        made.find(crate::work::TASK_FAILED, (0, 0), &[words.as_str()]);
    }
    // D111: an include read outside its package inside the workspace, a warning at its call.
    for finding in resolved.left_package.iter() {
        let words = core::str::from_utf8(&finding.message).map_err(|_| CliError::Text { what: "an included file's path is not UTF-8" })?;
        made.find(crate::names::include_outside::INCLUDE_OUTSIDE_PACKAGE, finding.at, &[words]);
    }
    for path in resolved.files.iter() {
        let input = task.inputs.iter().find(|input| input.name == surface(path)).ok_or(CliError::Text { what: "a surface, checked above, is gone" })?;
        made.reads.push((input.name, input.out));
    }
    for path in resolved.absent.iter() {
        made.reads.push((surface(path), blaze_drive::key::ABSENT));
    }
    // The bytes' digest of each data file in the read list (rule 21): an edit of the file moves
    // its record's out and asks this crate again; nothing else does.
    for (path, _) in resolved.data.iter() {
        let file = blaze_drive::Name::file(path.as_bytes());
        let input = task.inputs.iter().find(|input| input.name == file).ok_or(CliError::Text { what: "a data file's record, checked above, is gone" })?;
        made.reads.push((input.name, input.out));
    }
    for g in local.iter() {
        let input = have(names, task, *g).ok_or(CliError::Text { what: "a crate's input, checked above, is gone" })?;
        made.reads.push((input.name, input.out));
    }
    // Its script's run, by its out: an equal record moves nothing; another runs this crate.
    if let Some(script) = script {
        let input = task.inputs.iter().find(|input| input.name == script).ok_or(CliError::Text { what: "a crate's build script's record, checked above, is gone" })?;
        made.reads.push((input.name, input.out));
    }
    // The crate's link table as a part of its own, located by the table's digest
    // (native_links.rs `table_digest`): its reader, the program's native image, is moved only
    // when the table's bytes move, never by a re-solve that leaves them equal. An empty payload
    // is a second answer, which `hand_payload` refuses below.
    if !resolved.payload.is_empty() {
        let links = sections::sections(&resolved.payload)
            .map_err(|_| CliError::Text { what: "a crate's `CrateScope` payload, written by its resolution, does not open (names/scope.rs `answer`)" })?
            .links;
        made.parts.push(Part { name: Name::links_of(name), locate: Locate::Source(blaze_ladder::native_links::table_digest(links)), place: 0 });
    }
    // Last, after every step that can fail: a payload handed is gone from `names`.
    hand_payload(&mut names.made[i].1, &mut made)?;
    #[cfg(test)]
    MADE.with(|kept| kept.borrow_mut().push((name.subject, made.out, blaze_ids::ContentId::of("names scope test", made.payload_bytes()))));
    Ok(Did::Done(made))
}

/// Hands `resolved`'s payload to its record `made`, moved, never copied (rules 11 and 51): the
/// bytes stay in `Global`, where the crate's resolution made them, until the batch's answer is
/// written. A copy into the task's frame bounded the record by the frame: `core`'s payload,
/// 76,367,791 bytes, against a 64 MiB frame, failed `core` and left every crate above it
/// unchecked (2026-11-16 23:11). A crate's record is answered once a command; a second answer
/// finds the payload gone (a written one is never empty: [`sections::write`] puts its head
/// first) and is refused (rule 3), never answered with an empty record.
fn hand_payload(resolved: &mut Resolved, made: &mut Made<'_>) -> Result<(), CliError> {
    if resolved.payload.is_empty() {
        return Err(CliError::Text { what: "a crate's `CrateScope` record answered twice this command: its payload went to the first answer (names/scope.rs `hand_payload`)" });
    }
    let payload = core::mem::replace(&mut resolved.payload, blaze_own::Vec::new_in(Global));
    made.hand(payload)
}

/// [`MADE`], for a test.
#[cfg(test)]
pub(crate) fn scope_made() -> &'static std::thread::LocalKey<RefCell<std::vec::Vec<(blaze_ids::ContentId, blaze_ids::ContentId, blaze_ids::ContentId), Global>>> {
    &MADE
}

#[cfg(test)]
std::thread_local! {
    /// Each `CrateScope` record this thread answered: (subject, out, payload digest). The
    /// simulation runs a command's tasks on its test's thread, so a test reads its own.
    pub(crate) static MADE: RefCell<std::vec::Vec<(blaze_ids::ContentId, blaze_ids::ContentId, blaze_ids::ContentId), Global>> = const { RefCell::new(std::vec::Vec::new()) };
}

/// One `CrateScope` task on the main thread: the three steps back to back, the resolution in
/// a frame of its own. The pool runs the same steps (work.rs `run_batches`).
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn run<'f, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    names: &mut Option<Names<'f>>,
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
    env: &FetchEnv<'_>,
    macros: Option<&crate::proc_runner::MacroSide<'_, '_, '_>>,
    task: &Task<'_>,
    held: &'b Frame,
) -> Result<Did<'b>, CliError> {
    let own = Frame::with_reserve(SCOPE_JOBS, Tag::Other).map_err(|_| CliError::Text { what: "a crate scope's frame could not be reserved" })?;
    let resolution = {
        let mut round = ScopeRound::new(&own, false);
        match prepare(cx, root, store, names, lock, members, env, macros, task, &mut round, held)? {
            Prepared::Answer(did) => return Ok(did),
            Prepared::Made => None,
            Prepared::Resolve(job) => {
                let scratch = Frame::with_reserve(SCOPE_FRAME, Tag::Other).map_err(|_| CliError::Text { what: "a crate scope's frame could not be reserved" })?;
                Some(resolve(cx, root, job, &scratch).0)
            }
        }
    };
    let Some(names) = names.as_mut() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
    if let Some(resolution) = resolution
        && let Some(did) = settle(names, task, resolution, held)
    {
        return Ok(did);
    }
    answer(names, task, held)
}

#[cfg(test)]
mod tests {
    use super::sysroot_given;

    /// The real shelf's rows as the cutter writes them (blaze_toolchain::implied
    /// `crate_facts`): `core` (`#![no_core]`) names nothing; `alloc` (`#![no_std]`) core; `std`
    /// (itself `#![no_std]`) alloc and core; bstd (`cfg_attr(not(test), no_std)`, `extern
    /// crate alloc` in its root, `extern crate std;` in `mod rt`: blaze-std rt.rs) core, alloc
    /// and std, and its crate `blaze_ids` (std's prelude: `Vec<u64>`) core and std, edges
    /// written in its row though its manifest lists neither. A toolchain crate is given what
    /// its row names: blaze_ids std, and bstd std too (the Rust Reference, "Extern crate
    /// declarations": the item asks for its crate in whatever module it is written, binding
    /// the name there; `#![no_std]` only keeps `std` out of the extern prelude); a program
    /// crate (`app`) every sysroot crate. Before the cutter read every module file, bstd's row
    /// named no std, its `extern crate std;` bound nothing, and hello world's `blaze run`
    /// refused `std::thread::ThreadId` in `rt` NotFound (2026-11-17). CONTROL: runner 5's
    /// closure walk gave a toolchain crate the target sysroot whatever its row said. Prints
    /// `sysroot given: bstd [true, true, true, false], blaze_ids [true, false, true, false], app [true, true, true, true]`.
    #[test]
    fn a_toolchain_crate_is_given_what_its_row_names() {
        // World indexes: 0 core, 1 alloc, 2 std, 3 bstd, 4 blaze_ids, 5 app (a program crate).
        let deps: [&[(&str, usize)]; 6] = [
            &[],
            &[("core", 0)],
            &[("alloc", 1), ("core", 0)],
            &[("alloc", 1), ("blaze_ids", 4), ("core", 0), ("std", 2)],
            &[("core", 0), ("std", 2)],
            &[],
        ];
        let all = [Some(0), Some(1), Some(2), Some(3)];
        let given = |at: usize| sysroot_given(at != 5, at, all, deps[at]);
        assert_eq!(given(0), [false, false, false, false], "core");
        assert_eq!(given(1), [true, false, false, false], "alloc");
        assert_eq!(given(2), [true, true, false, false], "std: never itself");
        assert_eq!(given(3), [true, true, true, false], "bstd: its row names std, for `extern crate std;` in `mod rt`");
        assert_eq!(given(4), [true, false, true, false], "blaze_ids: its row names std");
        assert_eq!(given(5), [true, true, true, true], "a program crate: every sysroot crate");
        println!("sysroot given: bstd {:?}, blaze_ids {:?}, app {:?}", given(3), given(4), given(5));
    }

    /// A crate's payload larger than its task's frame is handed to its record, not copied: the
    /// record reads the payload's exact bytes at the address its resolution made them, the
    /// frame grew by 0, and a second answer of the crate is refused, never answered empty.
    /// CONTROL: the copy this replaced (`made.payload.extend_from_slice`) panics here, the frame
    /// refusing the payload's bytes as `core`'s 64 MiB task frame refused its 76 MB
    /// (2026-11-16 23:11); a copy into `Global` instead fails the address check. Prints
    /// `scope payload lent: 1048576 bytes, frame grew by 0`.
    #[test]
    fn a_scope_payload_larger_than_the_task_frame_is_handed_not_copied() {
        use bstd::mem::{Frame, Tag};
        use std::alloc::Global;
        const FRAME: usize = 64 << 10;
        const LEN: usize = 1 << 20;
        let bytes: std::vec::Vec<u8> = (0..LEN).map(|i| (i * 31 % 251) as u8).collect();
        let payload = blaze_own::Vec::from_slice_in(&bytes, Global);
        let at = payload.as_ptr();
        let mut resolved = super::Resolved {
            payload,
            out: blaze_ids::ContentId([7; 32]),
            files: std::vec::Vec::new(),
            absent: std::vec::Vec::new(),
            data: std::vec::Vec::new(),
            findings: blaze_own::Vec::new_in(Global),
            left_package: blaze_own::Vec::new_in(Global),
            failed: None,
            failed_calls: blaze_own::Vec::new_in(Global),
        };
        let frame = Frame::with_reserve(FRAME, Tag::Other).expect("a test frame");
        assert!(LEN > frame.reserved(), "the payload ({LEN} bytes) is larger than the frame ({} bytes)", frame.reserved());
        let name = blaze_drive::Name::unit_graph();
        let mut made = crate::work::Made::new_in(name, &frame);
        let before = frame.used();
        super::hand_payload(&mut resolved, &mut made).expect("the payload handed");
        let grew = frame.used() - before;
        assert!(made.payload_bytes() == &bytes[..], "the record reads the payload's exact bytes");
        assert_eq!(made.payload_bytes().as_ptr(), at, "the record reads the payload where it was made: it was copied");
        assert_eq!(grew, 0, "the task's frame grew by the payload");
        let mut again = crate::work::Made::new_in(name, &frame);
        assert!(super::hand_payload(&mut resolved, &mut again).is_err(), "a second answer of the crate was not refused");
        assert!(again.payload_bytes().is_empty(), "a refused answer holds no payload");
        println!("scope payload lent: {} bytes, frame grew by {grew}", made.payload_bytes().len());
    }
}
