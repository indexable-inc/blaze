//! One crate's names resolved on a pool worker ([`resolve`]) from a [`ScopeJob`] the engine's
//! names stage makes (engine_names.rs). A job shares RECORD BYTES and read-only views only: its
//! dependencies' exports records ([`DepExports`], read in place), the packs' views and the
//! corrections, the crate's environment lent to be copied ([`EnvLent`]). The worker reads the
//! checkout files the crate names itself ([`JobTexts`]), compiles the macros it calls and holds
//! everything it makes in its own task frame, freed when the task ends; what it returns is the
//! crate's record, owned.
//!
//! A crate's record is a function of its job: the same bytes whichever worker or order made it
//! (rule 25).

use core::cell::RefCell;
use std::alloc::Global;
use std::string::String;

use blaze_drive::Input;
use blaze_fetch::pack::PackView;
use blaze_id::CrateItemKey;
use blaze_own::{Alloc, Erased};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::body_scope::BlocksTally;
use blaze_scope::crate_scope::{CrateScopeError, resolve_crate_verified};
use blaze_scope::graph::Sysroot;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::CrateInput;
use blaze_scope::scope_record::Verified;
use blaze_scope::task::Par;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Frame, Tag};

use super::{OUT_DOMAIN, Resolved, scope_record, sections, slot_of};
use crate::CliError;
use crate::engine_names::{NamesOracle, OracleCrate, OracleLinks};
use crate::engine_records::ReadError;
use crate::job_texts::JobTexts;
use crate::store::Store;
use crate::work::Made;

/// The most one crate's resolution holds in its task frame: address space reserved, committed
/// as used (the largest crate of blaze's clone resolves in far less). Past it the task fails
/// alone, naming its frame (rule 20).
pub(crate) const SCOPE_FRAME: usize = 4 << 30;

/// The most a round's crate scope jobs hold together (their dependencies' exports records,
/// each once a round): reserved, committed as used, reset when the round's scope has joined.
pub(crate) const SCOPE_JOBS: usize = 4 << 30;

/// Read-only inputs assembled by the engine Names worker or by the legacy check coordinator.
pub(crate) struct ScopeJob<'j> {
    /// The requested width of this job's file outline scope.
    pub(crate) workers: core::num::NonZeroU32,
    /// Where the crate's findings are kept: they leave the worker with its result, so the
    /// request's shared heap, named (never a conjured one).
    pub(crate) kept: blaze_own::SharedAlloc<'static>,
    /// The crate's index in the world (its texts' `krate`).
    pub(crate) at: usize,
    /// Whether this crate belongs to a workspace member (only workspace members emit user-facing
    /// unresolved-path, missing-module, failed-call, include-outside, and test-mark findings).
    pub(crate) member: bool,
    /// Its input, its dependencies renumbered to `deps`' order.
    pub(crate) input: CrateInput<'j>,
    /// The exports record of each crate it reads, in its local order, each checked once where
    /// its bytes entered the command (engine_names.rs `checked`, rule 74): the job opens each in
    /// O(1) and checks none again.
    pub(crate) deps: &'j [Verified<'j>],
    pub(crate) sysroot: Sysroot,
    pub(crate) packs: crate::job_texts::Packs<'j>,
    pub(crate) corrected: &'j [(u32, &'j str, &'j str)],
    pub(crate) env: Option<EnvLent<'j, 'j>>,
    /// The command's proc-macro runner (`Names::procs`).
    pub(crate) procs: Option<&'j crate::proc_runner::ProcRunner<'static>>,
    /// `--names-oracle` (engine_names.rs [`NamesOracle`]): the crate's cells decided twice and
    /// compared; the clock the check is timed by.
    pub(crate) oracle: NamesOracle,
    pub(crate) clock: &'j bstd::clock::Clock,
    /// Status board for reporting live proc-macro expansion activity.
    pub(crate) board: Option<&'j crate::engine_status::Board<'j>>,
}

/// How a `CrateScope` job reads each dependency's exports section (rule 78: one seam). The
/// engine's names stage reads them in place through the request's one reader
/// (engine_records.rs `EngineRecords::exports`, engine_names.rs), so a wave's frame holds no
/// dependency's bytes. `krate` names the dependency for the names tests' watch (sections.rs
/// `watch`).
pub(crate) trait DepExports<'j, 's> {
    fn exports(&mut self, cx: &Cx, store: &mut dyn Store<'s>, input: &Input, krate: &str, frame: &'j Frame, alloc: Alloc<'j>) -> Result<&'j [u8], ReadError>;
}

/// A crate's resolution as its job made it, owned: what the engine's names stage files.
pub(crate) enum Resolution {
    /// Resolved (or failed, its failure filed in `resolved`); `ok`: the resolution itself
    /// succeeded; `unresolved`: its unresolved paths; `oracle`: under `--names-oracle`, the
    /// comparison its check reported (`None` when none did).
    Made { resolved: Resolved, ok: bool, unresolved: u32, oracle: Option<OracleCrate> },
    /// It needs these crates' records first.
    Need(std::vec::Vec<CrateItemKey, Global>),
}

/// Which of the sysroot crates `all` (core, alloc, std, bstd, by world index) the crate at
/// `at`, whose own dependencies are `own`, is given. A toolchain crate (`toolchain`): those its
/// shelf row names, which are its edges (the cutter writes the sysroot crates a root names as
/// real dependencies: blaze_toolchain::implied), read here and nowhere else. A crate of the
/// program: every one the world holds (which `std` it names is its `Implicit`'s; what its
/// root's `#![no_std]` drops is blaze-scope graph.rs `externs`'). Never the crate itself.
pub(crate) fn sysroot_given(toolchain: bool, at: usize, all: [Option<usize>; 4], own: &[(&str, usize)]) -> [bool; 4] {
    let mut given = [false; 4];
    for (slot, s) in given.iter_mut().zip(all) {
        let Some(s) = s else { continue };
        *slot = s != at && (!toolchain || own.iter().any(|(_, d)| *d == s));
    }
    given
}

/// The crate resolved, on whichever thread runs it, in `frame` (a pool task's own): its record
/// and read lists, owned, and what its frame held by the stretch of the solve that took it
/// ([`super::metered`]). Reads nothing but `job` and the checkout files the crate names.
pub(crate) fn resolve(cx: &Cx, root: &Dir<ReadWrite>, job: &ScopeJob<'_>, frame: &Frame) -> (Resolution, super::metered::ScopeBytes) {
    let failed_with = |words| failed_scope(words, job.kept);
    // Each macro call's working memory, beside the crate's frame (blaze-scope task/fill.rs
    // `Par::call_scratch`): reserved here, committed as used, rolled back after each call and
    // dropped with this resolve (its blocks back to the chunk cache). A frame that cannot be
    // reserved fails this crate, naming the frame (rules 3, 19), never a solve without it.
    let scratch = match Frame::with_reserve(CALL_SCRATCH, Tag::Expand) {
        Ok(scratch) => scratch,
        Err(error) => return (failed_with(std::format!("crate `{}`: its macro calls' scratch frame ({CALL_SCRATCH} bytes) could not be reserved: {error:?}", job.input.name)), super::metered::ScopeBytes::default()),
    };
    let metered = super::metered::Metered::new(frame, scratch, super::metered::CallScratch::Reset);
    let resolution = resolve_metered(cx, root, job, frame, &metered);
    let of = crate::phases::byte_rows::Label::of(format_args!("{}", job.input.name));
    (resolution, metered.finish(of))
}

/// Only a file outline's input text and edition cross this seam. The generic module lowerer
/// and the expansion fixpoint borrow the crate frame and its metering state on this worker.
struct FilePar<'p> {
    cx: &'p Cx,
    meter: &'p super::metered::Metered<'p>,
    heap: blaze_own::SharedAlloc<'static>,
    workers: core::num::NonZeroU32,
    /// `--names-oracle`: whether D116's check runs (blaze-scope task/fill.rs
    /// `Par::decide_check`), the clock it is timed by, and what it reported, copied out of the
    /// solve's frame onto `heap`.
    oracle: NamesOracle,
    now: &'p dyn Fn() -> u64,
    seen: RefCell<Option<Seen>>,
}

/// One crate's check as D116's `DecideCheck` reported it, before its items are named by path.
struct Seen {
    cells: u64,
    differences: u64,
    waits: u64,
    hops: u64,
    examples: blaze_own::Vec<SeenDifference, blaze_own::SharedAlloc<'static>>,
    links: SeenLinks,
}

struct SeenDifference {
    module: blaze_id::ItemKey,
    ns: blaze_surface::Namespace,
    name: blaze_own::String<blaze_own::SharedAlloc<'static>>,
    layer: blaze_scope::CellLayer,
    whole: blaze_scope::CellValue,
    decided: Option<blaze_scope::CellValue>,
}

enum SeenLinks {
    NotRun,
    Agreed { modules: u64, names: u64, capped: u64 },
    /// agree.rs's first disagreement, as its `Display` says it.
    Differ(blaze_own::String<blaze_own::SharedAlloc<'static>>),
}

impl Par for FilePar<'_> {
    fn decide_check(&self) -> Option<&dyn Fn() -> u64> {
        self.oracle.on().then_some(self.now)
    }

    fn decided(&self, _krate: &str, check: &blaze_scope::DecideCheck<'_, &str>) {
        let kept = self.heap;
        let mut examples = blaze_own::Vec::with_capacity_in(check.examples.len(), kept);
        for d in check.examples.iter() {
            examples.push(SeenDifference { module: d.module, ns: d.ns, name: blaze_own::String::from_str_in(d.name, kept), layer: d.layer, whole: d.whole, decided: d.decided });
        }
        let links = match check.links() {
            None => SeenLinks::NotRun,
            Some(Ok(agreed)) => SeenLinks::Agreed { modules: agreed.modules, names: agreed.names, capped: agreed.capped },
            Some(Err(differ)) => SeenLinks::Differ(blaze_own::format_in!(kept, "{differ}")),
        };
        *self.seen.borrow_mut() = Some(Seen { cells: check.cells, differences: check.differences, waits: check.waits, hops: check.hops, examples, links });
    }

    fn decide_plant(&self) -> blaze_scope::task::DecidePlant {
        self.oracle.plant()
    }

    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut blaze_own::AVec<'_, T>) {
        self.meter.map(n, f, out);
    }

    fn stage(&self, stage: blaze_scope::task::Stage) {
        self.meter.stage(stage);
    }

    fn expand_part(&self, part: blaze_scope::task::ExpandPart, name: &str, size: usize) {
        self.meter.expand_part(part, name, size);
    }

    fn call_scratch(&self, work: &mut dyn FnMut(blaze_own::Alloc<'_>)) -> blaze_scope::task::Scratch {
        self.meter.call_scratch(work)
    }

    fn outline_files<'a>(
        &self,
        krate: &'a str,
        files: &[(&'a str, &'a str)],
        edition: blaze_surface::Edition,
        alloc: blaze_own::Alloc<'a>,
        out: &mut blaze_own::AVec<'a, blaze_file_items::Outline<'a>>,
    ) -> Result<(), blaze_scope::resolve::NamesError<'a>> {
        if files.len() < 2 || self.workers.get().min(self.cx.workers()) == 1 {
            return self.meter.outline_files(krate, files, edition, alloc, out);
        }
        let scope = self.cx.scope();
        scope.narrow_workers(self.workers, bstd::task::Class::Foreground);
        let heap = self.heap;
        let mut jobs = blaze_own::Vec::with_capacity_in(files.len(), alloc);
        for &(path, text) in files {
            jobs.push((path, scope.may_overlap("names: outline module file", 64 << 10, move |_| {
                Ok::<_, core::convert::Infallible>(blaze_scope::resolve::SharedOutline::parse(text, edition, heap))
            })));
        }
        let mut failed = None;
        for (path, job) in jobs {
            match job.join(self.cx) {
                Ok(file) => out.push(file.into_frame(alloc)),
                Err(why) => { failed.get_or_insert((path, why)); }
            }
        }
        let joined = scope.join(self.cx);
        if let Some((path, why)) = failed.or_else(|| joined.err().map(|why| (krate, why))) {
            let words = blaze_own::format_in!(self.heap, "{why}");
            return Err(blaze_scope::resolve::NamesError::OutlineWorker { krate, path, why: blaze_own::Box::leak_static(words.into_boxed_str()) });
        }
        Ok(())
    }
}

/// The most one macro call's working memory holds ([`resolve`]'s call scratch): address space
/// reserved, committed as used. Past it the call's allocation fails as a crate frame's does.
pub(crate) const CALL_SCRATCH: usize = 4 << 30;

/// A crate whose resolution failed before or around its solve, `words` saying why.
fn failed_scope(words: String, kept: blaze_own::SharedAlloc<'static>) -> Resolution {
    Resolution::Made {
        resolved: Resolved {
            out: crate::work::failed_out(),
            payload: sections::write(0, [&[][..]; sections::SECTIONS]),
            files: std::vec::Vec::new(),
            absent: std::vec::Vec::new(),
            data: std::vec::Vec::new(),
            findings: blaze_own::Vec::new_in(kept),
            left_package: blaze_own::Vec::new_in(Global),
            failed: Some(words),
            failed_calls: blaze_own::Vec::new_in(Global),
            module_missing: blaze_own::Vec::new_in(kept),
            test_marks: blaze_own::Vec::new_in(Global),
            expansions: crate::work::Extras::none(),
        },
        ok: false,
        unresolved: 0,
        oracle: None,
    }
}

/// The frame the differential's `Modules` side is kept in between its two solves
/// (blaze-scope `ModulesSide`: each impl's row and header, the chains of the modules impls are
/// written in). Its size is the crate's impl count, never a solve's; outgrowing it is the
/// crate's verdict line (rule 18), never raised to fit.
const SIDE_FRAME: usize = 256 << 20;

/// D189 step 4's `--block-scopes=bodies` differential of one crate (blaze-scope crate_scope.rs
/// `block_scopes_modules_side` then `block_scopes_bodies_side`): its verdict line when it
/// differs or did not run, `None` when it agrees. Never in the crate's frame, and its two whole
/// solves never in one frame: the `Modules` solve runs in a frame of its own, what the
/// comparison reads of it is copied into [`SIDE_FRAME`], its frame is dropped, and only then
/// does the `Bodies` solve (with every held body's) run in a frame of its own. With both in one
/// [`SCOPE_FRAME`], tempo's `core` panicked at 4,411,162,624 bytes committed of 4,294,967,296;
/// the peak is now the larger of the two solves. Each half under its own `unwinding`, so a
/// panic or a frame it outgrows is this crate's verdict line, never the crate's job (rule 20).
/// The span is the proof it ran (rule 61).
#[allow(clippy::too_many_arguments, reason = "the crate job's parts the differential reads, each named")]
fn blocks_differential(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    job: &ScopeJob<'_>,
    env: Option<&blaze_scope::crate_env::CrateEnv<'_>>,
    callers: &[(usize, usize, blaze_expand::Edition)],
    metered: &super::metered::Metered<'_>,
    now: &dyn Fn() -> u64,
) -> (BlocksTally, Option<blaze_own::String<blaze_own::SharedAlloc<'static>>>) {
    let crate_name = job.input.name;
    let span = cx.span("crate scope: block scopes differential");
    let side_frame = match Frame::with_reserve(SIDE_FRAME, Tag::Nameres) {
        Ok(f) => f,
        Err(error) => return (BlocksTally::failed(), Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: the `Modules` side's frame ({SIDE_FRAME} bytes) could not be reserved: {error:?}"))),
    };
    let side_erased = Erased::new(&side_frame);
    let out = side_erased.bump();

    // The `Modules` solve, in its own frame, dropped at the end of this block.
    let modules = {
        let span = span.span("block scopes differential: the `Modules` solve");
        let own = match Frame::with_reserve(SCOPE_FRAME, Tag::Nameres) {
            Ok(own) => own,
            Err(error) => return (BlocksTally::failed(), Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: the `Modules` solve's frame ({SCOPE_FRAME} bytes) could not be reserved: {error:?}"))),
        };
        let ran = crate::work::unwinding(|| {
            let erased = Erased::new(&own);
            let bump = erased.bump();
            let alloc = bump.alloc();
            let texts = JobTexts {
                cx: &span,
                root,
                frame: &own,
                packs: job.packs,
                corrected: job.corrected,
                env: env.map(|env| (job.at, env)),
                envs: None,
                read: RefCell::new(std::vec::Vec::new()),
                unread: RefCell::new(blaze_own::Vec::new_in(&own)),
                data: RefCell::new(std::vec::Vec::new()),
                left: RefCell::new(std::vec::Vec::new()),
                procs: crate::job_texts::Procs::of(job.procs),
                own: Some(job.at),
                callers,
                by_need: None,
                run_stopped: core::cell::Cell::new(None),
                activity: job.board.map(|board| (board, job.clock)),
            };
            let par = FilePar { cx: &span, meter: metered, heap: job.kept, workers: job.workers, oracle: NamesOracle::Off, now, seen: RefCell::new(None) };
            // The differential's two solves take bytes and check them again: an oracle's path,
            // off the command's (`--block-scopes=bodies` only).
            let deps = blaze_own::AVec::from_iter_in(job.deps.iter().map(|proof| proof.bytes()), alloc);
            blaze_scope::crate_scope::block_scopes_modules_side(&job.input, &deps, job.sysroot, &texts, job.at, &par, Prepares::new_in(bump), bump, out)
                .map_err(|error| blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: {error}"))
        });
        match ran {
            Ok(Ok(side)) => side,
            Ok(Err(words)) => return (BlocksTally::failed(), Some(words)),
            Err(crate::work::Panicked) => {
                return (
                    BlocksTally::failed(),
                    Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: the `Modules` solve panicked in its own frame ({} bytes committed of {SCOPE_FRAME}; the side's frame {} of {SIDE_FRAME})", own.committed(), side_frame.committed())),
                );
            }
        }
    };

    // The `Bodies` solve and every held body's, in a frame of their own.
    let span = span.span("block scopes differential: the `Bodies` solve");
    let own = match Frame::with_reserve(SCOPE_FRAME, Tag::Nameres) {
        Ok(own) => own,
        Err(error) => return (BlocksTally::failed(), Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: the `Bodies` solve's frame ({SCOPE_FRAME} bytes) could not be reserved: {error:?}"))),
    };
    let ran = crate::work::unwinding(|| {
        let erased = Erased::new(&own);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let texts = JobTexts {
            cx: &span,
            root,
            frame: &own,
            packs: job.packs,
            corrected: job.corrected,
            env: env.map(|env| (job.at, env)),
            envs: None,
            read: RefCell::new(std::vec::Vec::new()),
            unread: RefCell::new(blaze_own::Vec::new_in(&own)),
            data: RefCell::new(std::vec::Vec::new()),
            left: RefCell::new(std::vec::Vec::new()),
            procs: crate::job_texts::Procs::of(job.procs),
            own: Some(job.at),
            callers,
            by_need: None,
            run_stopped: core::cell::Cell::new(None),
            activity: job.board.map(|board| (board, job.clock)),
        };
        let par = FilePar { cx: &span, meter: metered, heap: job.kept, workers: job.workers, oracle: NamesOracle::Off, now, seen: RefCell::new(None) };
        let deps = blaze_own::AVec::from_iter_in(job.deps.iter().map(|proof| proof.bytes()), alloc);
        match blaze_scope::crate_scope::block_scopes_bodies_side(&modules, &job.input, &deps, job.sysroot, &texts, job.at, &par, Prepares::new_in(bump), bump) {
            Ok(verdict) if verdict.agrees() => (BlocksTally::of(&verdict), None),
            Ok(verdict) => (BlocksTally::of(&verdict), Some(blaze_own::format_in!(job.kept, "{verdict}"))),
            Err(error) => (BlocksTally::failed(), Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: {error}"))),
        }
    });
    match ran {
        Ok(out) => out,
        Err(crate::work::Panicked) => (
            BlocksTally::failed(),
            Some(blaze_own::format_in!(job.kept, "block scopes differential: crate `{crate_name}`: DID NOT RUN: the `Bodies` solve panicked in its own frame ({} bytes committed of {SCOPE_FRAME})", own.committed())),
        ),
    }
}

fn resolve_metered(cx: &Cx, root: &Dir<ReadWrite>, job: &ScopeJob<'_>, frame: &Frame, metered: &super::metered::Metered<'_>) -> Resolution {
    let failed_with = |words| failed_scope(words, job.kept);
    let cx = cx.span("crate scope: resolve one crate's names");
    let erased = Erased::new(frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let crate_name = job.input.name;
    let env = match job.env {
        None => None,
        Some(lent) => match lent.copy_in(bump) {
            Ok(env) => Some(&*blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(env, frame))),
            Err(error) => return failed_with(std::format!("crate `{crate_name}`: its environment's build-script record did not open again: {error:?}")),
        },
    };
    let callers: &[(usize, usize, blaze_expand::Edition)] = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in([(job.at, job.at, job.input.edition)], frame));
    let texts = JobTexts {
        cx: &cx,
        root,
        frame,
        packs: job.packs,
        corrected: job.corrected,
        env: env.map(|env| (job.at, env)),
        envs: None,
        read: RefCell::new(std::vec::Vec::new()),
        unread: RefCell::new(blaze_own::Vec::new_in(frame)),
        data: RefCell::new(std::vec::Vec::new()),
        left: RefCell::new(std::vec::Vec::new()),
        procs: crate::job_texts::Procs::of(job.procs),
        own: Some(job.at),
        callers,
        by_need: None,
        run_stopped: core::cell::Cell::new(None),
        activity: job.board.map(|board| (board, job.clock)),
    };
    let prepares = Prepares::new_in(bump);
    let now = || crate::engine_times::ns(job.clock.now().since_start());
    let (done, seen) = {
        let solve = cx.span("crate scope: resolve_crate");
        let par = FilePar { cx: &solve, meter: metered, heap: job.kept, workers: job.workers, oracle: job.oracle, now: &now, seen: RefCell::new(None) };
        let done = resolve_crate_verified(&job.input, job.deps, job.sysroot, &texts, job.at, &par, prepares, bump);
        (done, par.seen.into_inner())
    };
    if let crate::job_texts::Packs::SyncLazy(lazy, _) = job.packs
        && let Some(why) = lazy.refused()
    {
        let words = bstd::mem::format_in!(frame, "crate `{crate_name}`: {why}");
        return failed_with(String::from(words.as_str()));
    }
    // The crate's comparison, its items named by path while its names are at hand.
    let oracle = match (job.oracle.on(), seen) {
        (true, Some(seen)) => Some(oracle_crate(crate_name, seen, done.as_ref().ok().map(|done| done.names), job.kept)),
        _ => None,
    };
    // `--block-scopes=bodies` (D189 step 4, proof 3): the crate solved again under each
    // block-scope strategy, its own texts read afresh (the crate's read list above is not
    // touched), and its impl tables compared. Every crate adds its tally to the oracle's
    // block-scopes count line (`o.blocks`); one that differs or did not run also gives its
    // verdict line (`o.blocks_line`), listed under its own cap (engine_check.rs). The span is the
    // proof it ran (rule 61).
    let oracle = match (job.oracle.blocks(), oracle) {
        (true, Some(mut o)) => {
            let (tally, line) = blocks_differential(&cx, root, job, env, callers, metered, &now);
            o.blocks = Some(tally);
            o.blocks_line = line;
            Some(o)
        }
        (_, o) => o,
    };
    metered.outside(super::metered::Outside::Record);
    // Each include that left its package inside the workspace (D111): a warning finding at
    // its call, placed in the crate's own files (none when the crate failed or is external).
    let left_package = match (job.member, &done) {
        (true, Ok(done)) => crate::names::include_outside::findings(done.names, &texts.left.borrow(), job.input.shown_from),
        _ => blaze_own::Vec::new_in(Global),
    };
    // The out of a resolved crate: its names record here, its files below ([`OUT_DOMAIN`]);
    // `None`: failed ([`crate::work::failed_out`]).
    // Each failed or refused macro call of the crate's own code (a `main` the implicit
    // `::std::main` did not write among them): an error finding at the call with its reason.
    let failed_calls = match (job.member, &done) {
        (true, Ok(done)) => crate::names::failed_call::findings(done.names, bump),
        _ => blaze_own::Vec::new_in(Global),
    };
    // Each `mod` of the crate's own code whose file is at neither place (or whose `#[path]`
    // names none): an error finding at the `mod` item (E0583; cfg decided by the walk). The
    // module is empty for the rest of the crate, which is still solved.
    let module_missing = match (job.member, &done) {
        (true, Ok(done)) => crate::names::module_missing::findings(done.names, job.kept),
        _ => blaze_own::Vec::new_in(job.kept),
    };
    // The crate's failed dependencies (each handed as an error record, engine_names.rs): a path
    // that fails behind one (written into it, or through a glob or import that failed into it)
    // waits on it, counted, never a finding (plan row A8, rule 19; blaze-scope waits_on.rs).
    let mut failed_deps: blaze_own::AVec<'_, blaze_scope::waits_on::FailedDep<'_>> = blaze_own::AVec::new_in(alloc);
    failed_deps.extend(job.input.deps.iter().filter_map(|(called, at)| {
        let (key, name) = blaze_scope::crate_scope::error_record_crate(job.deps.get(*at)?.bytes())?;
        Some(blaze_scope::waits_on::FailedDep { called: Some(*called), name, key })
    }));
    let (out, payload, findings, failed, ok, unresolved, test_marks, expansions) = match done {
        Ok(done) => match scope_record(&done, crate_name, job.member, &blaze_scope::waits_on::WaitsOn::of(done.names, &failed_deps, alloc), bump, job.kept) {
            Ok(record) => {
                // Both records (rule 21): a dependent reads the exports, but the crate's own
                // bodies read its inner names (front_load.rs), so an out over the exports alone
                // left a private item's removal moving no reader: its body was handed the record
                // that still resolved the item gone (`SourceError::ItemStale`, lane UN
                // 2026-11-17; signature_grain_tests `a_name_that_does_not_exist_is_an_error`).
                let mut out = blaze_ids::Hasher::new(OUT_DOMAIN);
                out.update(&(done.record.len() as u64).to_le_bytes()).update(&done.record);
                out.update(&(done.inner.len() as u64).to_le_bytes()).update(&done.inner);
                out.id(&record.expansions_out);
                (Some(out), record.payload, record.findings, record.tests_refused, true, record.unresolved, record.test_marks, record.expansions)
            }
            Err(unwritten) => (
                None,
                sections::write(0, [&[][..]; sections::SECTIONS]),
                blaze_own::Vec::new_in(job.kept),
                Some(std::format!("{unwritten}")),
                true,
                0,
                blaze_own::Vec::new_in(Global),
                crate::work::Extras::none(),
            ),
        },
        Err(CrateScopeError::Need(keys)) => return Resolution::Need(keys.iter().copied().collect()),
        // The crate's typed failure is its answer: a record whose out
        // says failed, with the files it read so far as its read list, so it stands until
        // one of them moves and a reader of it fails `Waited` on it.
        Err(error) => (
            None,
            sections::write(0, [&[][..]; sections::SECTIONS]),
            blaze_own::Vec::new_in(job.kept),
            Some(std::format!("crate `{crate_name}`: its names could not be resolved: {error:?}")),
            false,
            0,
            blaze_own::Vec::new_in(Global),
            crate::work::Extras::none(),
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
    // A file read `Unknown` (there, not module text: job_texts.rs `unread`) is a read of the
    // file as any other, so the answer (this job's failure) is asked again when it moves.
    for (path, _) in texts.unread.into_inner().iter() {
        read.push(String::from(*path));
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
        texts.data.into_inner().into_iter().map(|(path, from, _)| (String::from(path), slot_of(String::from(from), looked))).collect();
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
    Resolution::Made { resolved: Resolved { out, payload, files, absent, data, findings, left_package, failed, failed_calls, module_missing, test_marks, expansions }, ok, unresolved, oracle }
}

/// The crate's comparison, owned on `kept`: its counts, and a line for each disagreement its
/// check kept naming the crate, the module by path, the namespace, the name and both answers,
/// each found item by its path where `names` declares it (else its key).
fn oracle_crate(krate: &str, seen: Seen, names: Option<&blaze_scope::resolve::CrateNames<'_>>, kept: blaze_own::SharedAlloc<'static>) -> OracleCrate {
    let mut lines = blaze_own::Vec::with_capacity_in(seen.examples.len() + 1, kept);
    for d in seen.examples.iter() {
        let module = Module { key: d.module, names };
        let whole = Answer { value: Some(d.whole), names };
        let decided = Answer { value: d.decided, names };
        lines.push(blaze_own::format_in!(kept, "crate `{krate}` module {module} {} `{}` ({:?}): whole solve {whole}, on demand {decided}", d.ns.name(), d.name, d.layer));
    }
    let links = match seen.links {
        SeenLinks::NotRun => OracleLinks::NotRun,
        SeenLinks::Agreed { modules, names, capped } => OracleLinks::Agreed { modules, names, capped },
        SeenLinks::Differ(words) => {
            lines.push(blaze_own::format_in!(kept, "crate `{krate}` names records: {words}"));
            OracleLinks::Differ
        }
    };
    let modules = names.map_or(0, |names| (names.tree.modules.len() + names.made.len()) as u64);
    OracleCrate { krate: blaze_own::String::from_str_in(krate, kept), cells: seen.cells, differences: seen.differences, waits: seen.waits, hops: seen.hops, modules, links, lines, blocks: None, blocks_line: None }
}

/// A module, printed by its path (`krate::a::b`) where the crate's names hold it, else its key.
struct Module<'n, 'a> {
    key: blaze_id::ItemKey,
    names: Option<&'n blaze_scope::resolve::CrateNames<'a>>,
}

impl core::fmt::Display for Module<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.names.and_then(|names| names.path_of(self.key)) {
            Some(path) => write!(f, "`{path}`"),
            None => write!(f, "{:x}", self.key),
        }
    }
}

/// A decided cell's value, a found item by its path.
struct Answer<'n, 'a> {
    value: Option<blaze_scope::CellValue>,
    names: Option<&'n blaze_scope::resolve::CrateNames<'a>>,
}

impl core::fmt::Display for Answer<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use blaze_scope::CellValue;
        let item = |f: &mut core::fmt::Formatter<'_>, def: blaze_surface::Def| -> core::fmt::Result {
            match self.names.and_then(|names| item_path(names, def)) {
                Some((path, "")) => write!(f, "`{path}` ({:?})", def.kind),
                Some((module, name)) => write!(f, "`{module}::{name}` ({:?})", def.kind),
                None => write!(f, "{:?} {:x}", def.kind, def.key),
            }
        };
        match self.value {
            None => write!(f, "undecided"),
            Some(CellValue::NotFound) => write!(f, "not found"),
            Some(CellValue::Outside) => write!(f, "outside (an import stage 1 does not follow)"),
            Some(CellValue::Found(def, vis)) => {
                item(f, def)?;
                write!(f, " {vis:?}")
            }
            Some(CellValue::Ambiguous(def, vis)) => {
                write!(f, "ambiguous, the first ")?;
                item(f, def)?;
                write!(f, " {vis:?}")
            }
        }
    }
}

/// Where `def` is declared among the crate's modules and calls: its module's path and its name
/// (a module: its own path and no name); an item a macro call wrote, by the call's module.
fn item_path<'n, 'a: 'n>(names: &'n blaze_scope::resolve::CrateNames<'a>, def: blaze_surface::Def) -> Option<(&'n str, &'n str)> {
    if def.kind == blaze_surface::DefKind::Module {
        return Some((names.path_of(def.key)?, ""));
    }
    let declares = |decls: &blaze_surface::DeclsBuilder<'a, &'a str>| -> Option<&'a str> {
        decls.items.iter().find(|item| item.def.key == def.key).map(|item| item.name)
            .or_else(|| decls.macro_rules.iter().find(|m| m.def == def.key).map(|m| m.name))
    };
    for (module, (_, decls)) in names.decls.iter() {
        if let Some(name) = declares(decls) {
            return Some((names.path_of(*module)?, name));
        }
    }
    for (module, made) in names.made.iter() {
        if let Some(name) = declares(&made.decls) {
            return Some((names.path_of(*module)?, name));
        }
    }
    for (call, expansion) in names.expansions.iter() {
        if let Some(name) = declares(&expansion.decls) {
            return Some((names.path_of(call_module(names, *call)?)?, name));
        }
    }
    None
}

/// The module whose text, or whose call's expansion, holds macro call `call` (expansions
/// nest at most [`blaze_scope::MAX_EXPANSIONS`] deep: the walk is bounded by it, rule 18).
fn call_module<'a>(names: &blaze_scope::resolve::CrateNames<'a>, mut call: blaze_id::ItemKey) -> Option<blaze_id::ItemKey> {
    let holds = |decls: &blaze_surface::DeclsBuilder<'a, &'a str>, call: blaze_id::ItemKey| decls.calls.iter().any(|c| c.call == call);
    for _ in 0..blaze_scope::MAX_EXPANSIONS {
        if let Some((module, _)) = names.decls.iter().find(|(_, (_, decls))| holds(decls, call)) {
            return Some(*module);
        }
        if let Some((module, _)) = names.made.iter().find(|(_, made)| holds(&made.decls, call)) {
            return Some(*module);
        }
        call = *names.expansions.iter().find(|(_, expansion)| holds(&expansion.decls, call))?.0;
    }
    None
}

pub(crate) struct FindingText(pub(crate) &'static str);

impl Resolved {
    /// File the resolver's ordered diagnostics in either check driver's record.
    pub(crate) fn file_findings<A: core::alloc::Allocator + Copy>(&self, made: &mut crate::work::Record<A>) -> Result<(), FindingText> {
        let mut finding = |rule, at, bytes: &[u8], what| -> Result<(), FindingText> {
            let words = core::str::from_utf8(bytes).map_err(|_| FindingText(what))?;
            made.find(rule, at, &[words]);
            Ok(())
        };
        for (rule, f) in &self.findings {
            finding(*rule, f.at, &f.message, "a path's segment is not UTF-8")?;
        }
        for f in &self.failed_calls {
            finding(super::failed_call::FAILED_CALL, f.at, &f.message, "a failed call's words are not UTF-8")?;
        }
        for f in self.module_missing.iter() {
            finding(f.rule, f.at, &f.message, "a module file's path is not UTF-8")?;
        }
        for (rule, f) in &self.test_marks {
            finding(*rule, f.at, &f.message, "a test finding's words are not UTF-8")?;
        }
        if let Some(words) = &self.failed {
            finding(crate::work::TASK_FAILED, (0, 0), words.as_bytes(), "a crate's failure is not UTF-8")?;
        }
        for f in &self.left_package {
            finding(super::include_outside::INCLUDE_OUTSIDE_PACKAGE, f.at, &f.message, "an included file's path is not UTF-8")?;
        }
        Ok(())
    }
}

/// Hands `resolved`'s payload to its record `made`, moved, never copied (rules 11 and 51): the
/// bytes stay in `Global`, where the crate's resolution made them, until the batch's answer is
/// written. A copy into the task's frame bounded the record by the frame: `core`'s payload,
/// 76,367,791 bytes, against a 64 MiB frame, failed `core` and left every crate above it
/// unchecked (2026-11-16 23:11). A crate's record is answered once a command; a second answer
/// finds the payload gone (a written one is never empty: [`sections::write`] puts its head
/// first) and is refused (rule 3), never answered with an empty record.
pub(crate) fn hand_payload(resolved: &mut Resolved, made: &mut Made<'_>) -> Result<(), CliError> {
    if resolved.payload.is_empty() {
        return Err(CliError::Text { what: "a crate's `CrateScope` record answered twice this command: its payload went to the first answer (names/scope.rs `hand_payload`)" });
    }
    let payload = core::mem::replace(&mut resolved.payload, blaze_own::Vec::new_in(Global));
    made.hand(payload)?;
    // Its expansion records with it, moved: the directory in the payload names them, so they
    // go to the store after it (engine_store.rs `put_all`).
    made.hand_extras(core::mem::replace(&mut resolved.expansions, crate::work::Extras::none()))
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
            findings: blaze_own::Vec::new_in(blaze_own::testing::heap().shared()),
            left_package: blaze_own::Vec::new_in(Global),
            failed: None,
            failed_calls: blaze_own::Vec::new_in(Global),
            module_missing: blaze_own::Vec::new_in(blaze_own::testing::heap().shared()),
            test_marks: blaze_own::Vec::new_in(Global),
            expansions: crate::work::Extras::none(),
        };
        let frame = Frame::with_reserve(FRAME, Tag::Other).expect("a test frame");
        assert!(LEN > frame.reserved(), "the payload ({LEN} bytes) is larger than the frame ({} bytes)", frame.reserved());
        let name = blaze_drive::Name::unit_graph();
        let mut made = crate::work::Made::new_in(name, &frame);
        let before = frame.used();
        super::hand_payload(&mut resolved, &mut made).expect("the payload handed");
        let grew = frame.used() - before;
        assert!(made.payload_bytes().unwrap() == &bytes[..], "the record reads the payload's exact bytes");
        assert_eq!(made.payload_bytes().unwrap().as_ptr(), at, "the record reads the payload where it was made: it was copied");
        assert_eq!(grew, 0, "the task's frame grew by the payload");
        let mut again = crate::work::Made::new_in(name, &frame);
        assert!(super::hand_payload(&mut resolved, &mut again).is_err(), "a second answer of the crate was not refused");
        assert!(again.payload_bytes().unwrap().is_empty(), "a refused answer holds no payload");
        println!("scope payload lent: {} bytes, frame grew by {grew}", made.payload_bytes().unwrap().len());
    }
}
