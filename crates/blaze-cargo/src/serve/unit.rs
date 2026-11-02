//! The daemon's real build ([`Real`], the [`Steps`] of `blaze serve`; docs/v2/plan/lanes/cargo/notes.md
//! section 6): a job's units lowered into the front end, each checked, compiled and linked
//! by blaze's own code generator and linker, its build scripts, tests and program run as the
//! driver's sandboxed spawns.
//!
//! - [`graph`]: the job's unit graph as flat columns, built once.
//! - [`files`]: where a unit's files are (the mounts), and the names rustc gives them.
//! - [`compile`]: [`Front::compile`] and [`Front::fresh`]: diagnostics, codegen, the link,
//!   the files cargo would write, a unit's record in the store.
//! - [`codegen`]: a unit's codegen policy and roots, and its link.
//! - [`artifact`]: cargo's `compiler-artifact` of a unit.
//! - [`script`]: a build script's run and what its directives do, as cargo applies them.
//! - [`test`]: the tests of a build, on one queue, the reached ones only; with `--ub`, each
//!   run in blaze's MIR evaluator under the checker of undefined behavior, nothing compiled.
//! - [`run`]: the program `cargo run` picks, run in the tests' sandbox.
//! - [`reach`]: what a change reaches, from the database's facts.
//! - [`pack`]: `blaze pack`'s pack of a checked program, for blaze's in-browser compiler.
//! - [`say`]: cargo's own lines (`error: ...`, `warning: ...`) as diagnostics.
//!
//! The front end is written by the loop alone, through the job's [`UnitWriter`], and read by
//! each task through the [`UnitReader`] it was posted with, a shared reference: no worker
//! locks to read it, and a write waits for every reader before it (`work`'s docs).
//!
//! Nothing here runs rustc or cargo, and nothing user code runs is outside the sandbox but
//! what `BLAZE_SANDBOX=off` and the manifest's opt-outs take out (docs/cargo.md).

#![forbid(unsafe_code)]

mod artifact;
mod checks;
mod codegen;
mod compile;
mod deps;
mod doc;
mod error;
mod files;
mod graph;
mod harness;
mod intent;
mod layout;
mod memo;
mod pack;
mod perf;
mod reach;
mod reads;
mod run;
mod say;
mod script;
pub(crate) use script::compile_of;
mod test;

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

#[expect(clippy::disallowed_types, reason = "a job's slots, one per unit, each set once by the unit's task: per job, not global")]
use std::sync::OnceLock;

use base_db::FileId;
use blaze_api::{ApiError, Artifact, CrateType, Reach, Response, Table, TargetKind, TestResult, Ub, UnitOf, UnitTarget};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, Tag};
use blaze_ids::ContentId;
use blaze_units::{
    query::{BuildConfig, Fetch, Units},
    schema::{Mode, Unit},
};
use blaze_lower::{Crates, Db, Lower, Lowered, ProcMacroLoadResult, Ran};
use parking_lot::Mutex;

use crate::serve::{
    Paths,
    fetch::Fetcher,
    work::{
        Broke, Compiled, Failed, Front, FrontEnd, Goal, Image, JobInput, Kept, KeptDb, KeptPut, Library, Phase, Put, RunPuts,
        Runs, Selection, Source, SourceFile, Steps, Writer, applying,
    },
};

/// The build `blaze serve` runs: one for the daemon's life, shared by every job.
pub struct Real {
    fetcher: Fetcher<blaze_registry::http::HyperHttp>,
    shared: Arc<Shared>,
}

/// What every job's front shares: the daemon's paths, what was read of the machine's files,
/// the code generator's memo. The images kept linked between jobs are the loop's
/// ([`Image`]).
pub(crate) struct Shared {
    pub(crate) state: PathBuf,
    pub(crate) launcher: PathBuf,
    /// The directory of blaze's stand-ins for the tools cargo names to a build script:
    /// `rustc` and `rustdoc`, each blaze's own executable under that name.
    pub(crate) tools: PathBuf,
    /// The code generator's memo, which each job takes at its start ([`Job::memo`]).
    pub(crate) memos: Arc<memo::Memos>,
    /// What `-C target-cpu=native` is on this machine; none without LLVM.
    pub(crate) host_cpu: Option<blaze_units::stdlib::target::HostCpu>,
    /// Test selection's one selector per suite, for the daemon's life
    /// (fastcompile-sol-select 1): each test run takes its suites' and gives them back.
    pub(crate) selectors: test::image::Selectors,
}

impl Real {
    pub fn new(paths: &Paths) -> Result<Real, String> {
        let fetcher = Fetcher::new(paths.state.join("fetch"))?;
        let tools = paths.state.join("tools");
        stand_ins(&tools, &paths.launcher)?;
        let shared = Shared {
            state: paths.state.clone(),
            launcher: paths.launcher.clone(),
            tools,
            memos: memo::Memos::new(),
            host_cpu: hir_ty_codegen::host_cpu().map(|(name, features)| blaze_units::stdlib::target::HostCpu {
                arch: std::env::consts::ARCH.to_owned(),
                name,
                features,
            }),
            selectors: test::image::Selectors::default(),
        };
        Ok(Real { fetcher, shared: Arc::new(shared) })
    }
}

/// blaze's executable linked as `rustc` and `rustdoc` in `dir`: what a build script is given as
/// `RUSTC` and `RUSTDOC`, so a probe of them (`autocfg`, `version_check`) is answered by blaze
/// and no rustc runs (docs/no-rustc.md).
fn stand_ins(dir: &std::path::Path, launcher: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for name in ["rustc", "rustdoc"] {
        let at = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        match std::fs::read_link(&at) {
            Ok(to) if to == launcher => continue,
            Ok(_) => std::fs::remove_file(&at).map_err(|error| format!("{}: {error}", at.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", at.display())),
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(launcher, &at).map_err(|error| format!("{}: {error}", at.display()))?;
        #[cfg(windows)]
        std::fs::hard_link(launcher, &at).map_err(|error| format!("{}: {error}", at.display()))?;
    }
    Ok(())
}

impl Steps for Real {
    type Front = JobFront;
    type Writer = UnitWriter;
    type Reader = UnitReader;
    type KeptFront = KeptLowered;
    type Linked = codegen::Linked;
    type Runs = UnitRuns;

    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        self.fetcher.fetch(fetch)
    }

    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError> {
        let at = || Some("the standard library's sources".to_owned());
        let library = crate::toolchain::library::fetch().map_err(|what| ApiError::new(what, at()))?;
        let in_dir = |what| ApiError::new(what, Some(library.dir.display().to_string()));
        let named = blaze_units::stdlib::library_at(&library.commit);
        let mut hasher = blaze_ids::Hasher::new("blaze v2 serve library fingerprint");
        hasher.part(library.digest.as_bytes());
        library_fingerprint(&library.dir, "", &mut hasher).map_err(in_dir)?;
        let fingerprint = format!("{:x}", hasher.finish());
        if kept.iter().any(|(it, mounted)| *it == fingerprint && *mounted == named) {
            return Ok(Library { at: named, fingerprint, source: None });
        }
        let mut files = Vec::new();
        library_files(&library.dir, "", &mut files).map_err(in_dir)?;
        blaze_ids::names::sort_by_name(&mut files, |it| it.path.as_bytes());
        Ok(Library { at: named, fingerprint, source: Some(Source { files, under: String::new() }) })
    }

    fn lease(&self, units: &blaze_units::query::Units, checkout: &str, builds: &str) -> crate::serve::work::Leased {
        crate::serve::job::lease_build_dirs(units, checkout, builds)
    }

    fn store_kept(&self, kept: &mut KeptLowered) -> Result<Vec<KeptPut>, ApiError> {
        if kept.stored {
            return Ok(Vec::new()); // alloc: none
        }
        let later = std::mem::take(&mut kept.later);
        let deps = kept.deps.take();
        let stored = cancellable(|| deps::store(&kept.lowered, deps, &mut kept.session, &later, &kept.root));
        let (puts, flushes) = match stored {
            Ok(stored) => stored?,
            // A cancel of the front end's database or of the build graph, caught as the
            // lowering's are: what it stored so far is not all of it, so the front end is
            // dropped ([`super::job`]'s `stored_kept`) and the next build lowers it anew.
            Err(Failed::Again) => {
                // alloc: an error, once
                let what = format!(
                    "storing what the build of {} left for the store was cancelled partway (a cancel of its \
                     front end's database or of the build graph), so its front end is dropped and the \
                     checkout's next build lowers it anew",
                    kept.root
                );
                return Err(ApiError::new(what, Some(kept.root.clone()))); // clone: an error, once
            }
            Err(Failed::Error(error)) => return Err(error),
        };
        kept.flushed = Some(flushes);
        kept.stored = true;
        Ok(puts)
    }

    fn lower(&self, input: &JobInput, kept: Option<KeptLowered>) -> Result<FrontEnd<Real>, Failed> {
        if input.cancelled.load(Ordering::Relaxed) {
            return Err(Failed::Again);
        }
        let writer = cancellable(move || -> Result<(UnitWriter, Vec<Put>), Failed> {
            let mut kept = kept;
            // Held until this job's front end is kept with its own: what the head's last build
            // used stays in the memo for this one.
            let held = kept.as_mut().and_then(|it| it.memo.take());
            let puts = Vec::new(); // alloc: none
            // What the head's last build left for the store was written as it was kept
            // ([`Steps::store_kept`]), before that build answered; a front end that was not is
            // never lowered into, as its images and outcomes would be lost (rule 3).
            let mut flushed = None;
            if let Some(it) = &mut kept {
                if !it.stored {
                    let what = "the head's last front end is lowered into, and what its build left for the store was never written (`Steps::store_kept`)";
                    return Err(Failed::Error(ApiError::new(what, Some(it.root.clone())))); // clone: an error, once
                }
                flushed = it.flushed.take();
            }
            // Whether a record retracting a value verified across checkouts was published since
            // the kept front end was lowered (rule 18): its database's tracked expansions and
            // served images would keep such a value, so it is not lowered into, and the build
            // lowers afresh, where `deps::serve` works every verified image's reads out again
            // and an expansion is looked up again. Read before this job expands anything, so a
            // record published while it runs moves the epoch past the one its front end is kept
            // at. One atomic load (`Keeping::retractions_now`).
            let retractions = input.macros.keeps().retractions_now();
            // A kept front end that is not lowered into is let go by name: the build lowers
            // anew, and the daemon's log and the profile say which part of its inputs moved.
            let kept = match kept {
                Some(it) => match unkept(&it, input, retractions) {
                    None => Some(it),
                    Some(why) => {
                        let_go(it, &why);
                        None
                    }
                },
                None => None,
            };
            let lower = || Lower {
                units: &input.units,
                config: &input.config,
                files: &input.files,
                root: input.root,
                host_cpu: self.shared.host_cpu.as_ref(),
            };
            let failed = |error| Failed::Error(lower_error(error));
            // A front end kept and moved to the view keeps what it was served: only crates of
            // the build's dependencies were, and moving reads again only the view's files.
            let (mut lowered, session, seeded) = match kept {
                Some(KeptLowered { session, mut lowered, seeds, .. }) => match lowered
                    .refresh(lower(), |db| seed(db, &seeds))
                    .map_err(failed)?
                {
                    true => (lowered.into_inner(), Some(session), seeds),
                    false => {
                        let why = "the view moves the workspace's root or what its root manifest sets, which every crate is made with";
                        let checkout = input.config.invocation.root.as_str();
                        {
                            let _span = tracing::info_span!(target: "blaze", "kept front end let go", checkout, why).entered();
                            eprintln!("blaze serve: the kept front end of {checkout} is let go, and its build lowers anew: {why}");
                            drop((session, lowered, seeds));
                        }
                        (blaze_lower::lower(lower()).map_err(failed)?, None, Vec::new())
                    }
                },
                None => (blaze_lower::lower(lower()).map_err(failed)?, None, Vec::new()),
            };
            // Before any key is resolved: two crates of one key would take each other's items.
            blaze_items::key::check_crate_keys(lowered.db())
                .map_err(|clash| Failed::Error(ApiError::new(format!("blaze-items: {clash}"), None)))?; // alloc: an error, once
            let fresh = session.is_none();
            if fresh {
                blaze_item_hash::impls::install(lowered.served_db());
            }
            let mut session = match session {
                Some(session) => session,
                None => deps::session(&lowered, &input.store, &input.macros, &input.config.invocation.env)?,
            };
            let mut job = Job::new(input.clone(), lowered.crates().clone(), self.shared.clone(), held)?;
            job._seeded = seeded;
            let job = Arc::new(job);
            let waits = graph::Waits::new(&job.graph);
            // After the seeds (the root's parse is seeded as on disk), before any identity:
            // a test crate is identified by its root with the harness.
            harness::write(&mut lowered, &job, (0..job.graph.len() as u32).filter(|&unit| waits.ready(unit)))?;
            for unit in 0..job.graph.len() as u32 {
                if unit % 64 == 0 && input.cancelled.load(Ordering::Relaxed) {
                    return Err(Failed::Again);
                }
                if waits.ready(unit) {
                    identify(&mut lowered, &job, unit);
                }
            }
            let mut deps = None;
            if fresh {
                deps::serve(&mut lowered, &job, &deps::ready(&job, &waits), &mut session, &mut deps)?;
            }
            let writer = UnitWriter {
                session,
                front: Held::new(Arc::new(lowered), "a job's front end"),
                job,
                waits,
                ready: Vec::new(),
                runs: Vec::new(),
                macros: Vec::new(),
                deps,
                flushed,
                broke: Vec::new(),
                broken: false,
                retractions,
            };
            Ok((writer, puts))
        })??;
        let (writer, puts) = writer;
        let front = Arc::new(JobFront(writer.job.clone()));
        Ok(FrontEnd { front, writer, puts })
    }
}

/// Why job `input`'s lowering does not lower into the kept front end `kept`, by the part of
/// its inputs that moved; `None` when it does. `retractions` is the epoch read now
/// ([`proc_macro_api::kept::Keeping::retractions_now`]).
fn unkept(kept: &KeptLowered, input: &JobInput, retractions: u64) -> Option<String> {
    if !Arc::ptr_eq(&kept.units, &input.units) && kept.units != input.units {
        // alloc: a reason, once per front end let go
        return Some(format!("the plan's units moved ({})", units_differ(&kept.units, &input.units)));
    }
    if let Some(part) = blaze_lower::config_differs(&kept.config, &input.config) {
        // alloc: a reason, once per front end let go
        return Some(format!("its build config's {part} moved"));
    }
    if kept.retractions != retractions {
        // alloc: a reason, once per front end let go
        return Some(format!(
            "a record retracting a value verified across checkouts was published since it was lowered \
             (retractions epoch {} then, {retractions} now; rule 18)",
            kept.retractions
        ));
    }
    None
}

/// The first part of two plans' units that differs, by name. Every field is named, so one
/// added to [`Units`] is named here too.
fn units_differ(kept: &Units, now: &Units) -> &'static str {
    let Units { graph, plans, packages, lock, warnings, target_dir, build_dir, finished, link_dirs, sandboxed_tests } = kept;
    if graph != &now.graph {
        "the unit graph"
    } else if plans != &now.plans {
        "a unit's plan"
    } else if packages != &now.packages {
        "the packages"
    } else if lock != &now.lock {
        "the lock file to write"
    } else if warnings != &now.warnings {
        "the load's warnings"
    } else if target_dir != &now.target_dir {
        "the target directory"
    } else if build_dir != &now.build_dir {
        "the build directory"
    } else if finished != &now.finished {
        "the profile"
    } else if link_dirs != &now.link_dirs {
        "the build-script link directories"
    } else if sandboxed_tests != &now.sandboxed_tests {
        "whether tests run sandboxed"
    } else {
        "nothing compared"
    }
}

/// Lets go of the kept front end `kept`, which the build does not lower into because of
/// `why`, inside a span naming its checkout and why (dropping a whole database takes time),
/// and says so in the daemon's log.
fn let_go(kept: KeptLowered, why: &str) {
    let checkout = kept.root.as_str();
    let _span = tracing::info_span!(target: "blaze", "kept front end let go", checkout, why).entered();
    eprintln!("blaze serve: the kept front end of {checkout} is let go, and its build lowers anew: {why}");
    drop(kept);
}

/// Hands each of the kept `seeds` to its file as its next parse
/// ([`base_db::EditionedFileId::seed_parse`]), inside the refresh's write once the edit's
/// commit's texts are set and before its maintainers parse them. A seed refused (the text set
/// is not the one its tree was made of, or the file was parsed first) is an error naming the
/// file, never a whole parse in its place.
fn seed(db: &base_db::Db, seeds: &[blaze_intent::Seed]) -> Result<(), blaze_lower::LowerError> {
    for it in seeds {
        if !base_db::EditionedFileId::new(db, it.file, it.edition).seed_parse(db, it.content, &it.parse) {
            // alloc: an error path.
            let at = db.path_for_file(it.file).map_or_else(|| format!("{:?}", it.file), |path| path.to_string());
            let what = format!(
                "the edit's parse of the file was not taken (its text is not {}, or it was parsed first): it would be parsed again whole",
                it.content
            );
            return Err(blaze_lower::LowerError { what, at: Some(at) });
        }
    }
    Ok(())
}

fn lower_error(error: blaze_lower::LowerError) -> ApiError {
    ApiError::new(error.what, error.at)
}

impl KeptDb for KeptLowered {
    fn db(&self) -> Option<&Db> {
        Some(self.lowered.db())
    }
}

/// A job's front end as the loop keeps it ([`Writer::keep`]), with what it was lowered from:
/// the next build of the head lowers into it only when it lowers the same.
pub struct KeptLowered {
    /// What its database keeps of the store ([`deps::session`]), its goals flushed as it is
    /// stored ([`Steps::store_kept`]). Declared first: its goal store is uninstalled before the
    /// database it is keyed to drops.
    session: deps::Session,
    lowered: Held<Lowered>,
    units: Arc<Units>,
    config: Arc<BuildConfig>,
    /// What it was served with, until it is stored ([`Steps::store_kept`]).
    deps: Option<deps::Kept>,
    /// The epoch of retractions ([`proc_macro_api::kept::Keeping::retractions_now`]) its job
    /// was lowered at: the next build lowers into it only at the same one.
    retractions: u64,
    /// The dependency crates its build checked, and the workspace root their outcomes'
    /// paths are named from, until it is stored ([`Steps::store_kept`]).
    later: Vec<deps::Later>,
    root: String,
    /// Whether what its build left for the store was written ([`Steps::store_kept`]): the
    /// next build lowers into it only then.
    stored: bool,
    /// What its store's flushes did, for the `-v` note of the next build lowering into it.
    flushed: Option<deps::Flushes>,
    /// The trees its job's last edit reparsed in place, for the next build to seed its parses
    /// with when it moves to the edit's commit.
    seeds: Vec<blaze_intent::Seed>,
    /// What its job's builds used of the code generator's memo, kept there while it is kept;
    /// taken by the next job of the head.
    memo: Option<memo::Held>,
}

/// What a task reads the front end through ([`Steps::Reader`]): a shared reference to the job's
/// lowered front end, as it stood when the loop posted the task. Its database is read by
/// reference, never cloned.
pub struct UnitReader {
    front: Arc<Lowered>,
    /// The dependencies' check outcomes its database was served ([`deps::serve`]): written
    /// with the front end, only while no reader holds it.
    checks: Arc<blaze_diagnostics::cache::Served>,
}

impl UnitReader {
    pub(crate) fn db(&self) -> &Db {
        self.front.db()
    }

    pub(crate) fn checks(&self) -> &blaze_diagnostics::cache::Served {
        &self.checks
    }

    /// The job's files by path.
    pub(crate) fn paths(&self) -> &Arc<HashMap<FileId, String>> {
        self.front.paths()
    }
}

/// The job's front end as only the loop writes it: the one reference not lent to a reader,
/// the writes asked for and not yet applied, and as each build script's run is applied, the
/// identities of the units it was the last run in the closure of.
pub struct UnitWriter {
    /// What the front end's database keeps of the store ([`deps::session`]), declared first
    /// as in [`KeptLowered`].
    session: deps::Session,
    front: Held<Arc<Lowered>>,
    job: Arc<Job>,
    waits: graph::Waits,
    /// The units the runs of one apply made ready, reused from apply to apply.
    ready: Vec<u32>,
    /// Build scripts' runs asked for, checked, in the order asked.
    runs: Vec<(usize, Ran)>,
    /// Proc macros' loads asked for, checked, in the order asked.
    macros: Vec<(usize, ProcMacroLoadResult)>,
    /// What a front end freshly lowered was served with ([`deps::serve`]).
    deps: Option<deps::Kept>,
    /// What storing the kept front end this one lowered into did with its goals and bodies'
    /// steps ([`deps::store`]); None when nothing was kept.
    flushed: Option<deps::Flushes>,
    /// The writes that panicked as they were applied, not yet taken ([`Writer::broke`]).
    broke: Vec<Broke>,
    /// Whether a write ever panicked: the front end may be half written, and is not kept.
    broken: bool,
    /// The epoch of retractions its job was lowered at, kept with it ([`KeptLowered`]).
    retractions: u64,
}

/// `broke`, a panic of a step of an apply for each of the units `ready`, kept once for each.
fn broke_each(into: &mut Vec<Broke>, broke: Broke, ready: &[u32]) {
    for &unit in ready {
        // clone: a panic, once per unit it fails
        into.push(Broke { unit: unit as usize, what: broke.what, panic: broke.panic.clone() });
    }
}

impl Writer<Real> for UnitWriter {
    fn reader(&self) -> Option<UnitReader> {
        (!self.waiting()).then(|| UnitReader { front: Arc::clone(&self.front), checks: self.session.checks() })
    }

    fn ran(&mut self, run: usize, ran: Ran) -> Result<(), ApiError> {
        self.front.check_ran(run, &ran).map_err(lower_error)?;
        self.runs.push((run, ran));
        Ok(())
    }

    fn macros(&mut self, unit: usize, macros: ProcMacroLoadResult) -> Result<(), ApiError> {
        self.front.check_macros(unit).map_err(lower_error)?;
        self.macros.push((unit, macros));
        Ok(())
    }

    fn waiting(&self) -> bool {
        !self.runs.is_empty() || !self.macros.is_empty()
    }

    fn apply(&mut self) -> Result<bool, ApiError> {
        if !self.waiting() {
            return Ok(true);
        }
        let Some(lowered) = Arc::get_mut(&mut self.front) else { return Ok(false) };
        self.ready.clear();
        for (unit, macros) in self.macros.drain(..) {
            match applying(unit, "a proc macro's load", || lowered.macros(unit, macros)) {
                Ok(done) => {
                    done.map_err(lower_error)?;
                    self.ready.push(unit as u32);
                }
                Err(broke) => self.broke.push(broke),
            }
        }
        let (job, waits, ready) = (&self.job, &self.waits, &self.ready);
        if let Some(first) = ready.first() {
            match applying(*first as usize, "a proc macro's load", || harness::macros_applied(lowered, job, waits, ready)) {
                Ok(done) => done?,
                Err(broke) => broke_each(&mut self.broke, broke, &self.ready),
            }
        }
        self.ready.clear();
        for (run, ran) in self.runs.drain(..) {
            let from = self.ready.len();
            let (job, waits, ready) = (&self.job, &mut self.waits, &mut self.ready);
            let applied = applying(run, "a build script's run", || -> Result<(), ApiError> {
                lowered.ran(run, &ran).map_err(lower_error)?;
                waits.applied(run as u32, ready);
                harness::write(lowered, job, ready[from..].iter().copied())?;
                for &unit in &ready[from..] {
                    identify(lowered, job, unit);
                }
                Ok(())
            });
            match applied {
                Ok(done) => done?,
                Err(broke) => {
                    self.ready.truncate(from);
                    self.broke.push(broke);
                }
            }
        }
        // A front end freshly lowered was served; the units these runs made ready are served too.
        if self.deps.is_some() && !self.ready.is_empty() {
            let (job, ready, session, kept) = (&self.job, &self.ready, &mut self.session, &mut self.deps);
            match applying(ready[0] as usize, "serving its dependencies", || deps::serve(lowered, job, ready, session, kept)) {
                Ok(done) => done?,
                Err(broke) => broke_each(&mut self.broke, broke, &self.ready),
            }
        }
        Ok(true)
    }

    fn broke(&mut self) -> Option<Broke> {
        let broke = self.broke.pop();
        self.broken |= broke.is_some();
        broke
    }

    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed> {
        let Some(lowered) = Arc::get_mut(&mut self.front) else {
            let what = "the front end's tables are measured with no task reading it, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| lowered.tables())
    }

    fn finished(&mut self, ok: bool, built: &[u32]) -> Result<Vec<blaze_api::Diagnostic>, ApiError> {
        self.deps.as_ref().map_or(Ok(()), deps::differences)?;
        deps::item_errors(&self.session)?;
        Ok(codegen::unapplied(&self.job, ok, built)?.into_iter().map(warning).collect())
    }

    fn served(&self) -> Result<Vec<blaze_api::ServedCrate>, ApiError> {
        deps::served(self.front.db(), &self.job, &self.session)
    }

    fn goals_noted(&self) -> Option<blaze_api::Diagnostic> {
        deps::noted(self.flushed.as_ref(), self.session.item_steps().as_ref())
    }

    fn keep(mut self) -> Result<KeptLowered, ApiError> {
        if !self.apply()? {
            let what = "the front end is kept for the head's next build with no task reading it, and one still does";
            return Err(self.refused(what));
        }
        if self.broken || !self.broke.is_empty() {
            let what = "the front end is not kept for the head's next build: a write to it panicked, and it may be half written";
            return Err(self.refused(what));
        }
        let UnitWriter { session, front, job, deps, retractions, .. } = self;
        let Some(lowered) = Arc::into_inner(front.into_inner()) else {
            let what = "the front end is kept for the head's next build, and another holder of it is still alive";
            return Err(ApiError::new(what, None));
        };
        let later = std::mem::take(&mut *job.later.lock());
        let seeds = std::mem::take(&mut *job.seeds.lock());
        // alloc: once per front end kept, every entry its units' builds used
        let used: Vec<ContentId> = job.memo_used.iter().filter_map(|it| it.get()).flatten().copied().collect();
        let memo = Some(job.shared.memos.hold(used));
        Ok(KeptLowered {
            session,
            lowered: Held::new(lowered, "a kept front end"),
            units: job.input.units.clone(),
            config: job.input.config.clone(),
            deps,
            retractions,
            later,
            root: job.mounted.root().to_owned(), // alloc: once per front end kept
            stored: false,
            flushed: None,
            seeds,
            memo,
        })
    }
}

impl UnitWriter {
    /// The error of a front end [`Writer::keep`] refuses, which it lets go of here: the panic
    /// of that drop, if it panics, is said with the refusal and fails the job with it.
    fn refused(self, what: &str) -> ApiError {
        let UnitWriter { session, front, .. } = self;
        drop(session);
        match front.let_go() {
            Ok(()) => ApiError::new(what, None),
            // alloc: an error, once
            Err(panic) => ApiError::new(format!("{what}; letting go of it then {panic}"), None),
        }
    }
}

/// A front end's database as the loop holds it ([`UnitWriter`], [`KeptLowered`]). The last
/// drop of one drops a whole query storage, which may panic (a table it can't walk), and a
/// panic on the loop would end every request the daemon serves (rule 15). Its drop runs inside
/// a catch: a panic there is said in the daemon's log, naming what was let go, and where a job
/// can still fail with it, [`Held::let_go`] gives it to that job.
pub(crate) struct Held<T> {
    it: Option<T>,
    what: &'static str,
}

impl<T> Held<T> {
    fn new(it: T, what: &'static str) -> Held<T> {
        Held { it: Some(it), what }
    }

    fn into_inner(mut self) -> T {
        self.it.take().expect("a front end held is there until it is let go")
    }

    /// Drops it now, inside a catch: the panic, if the drop panics.
    fn let_go(mut self) -> Result<(), super::plane::Panic> {
        self.it.take().map_or(Ok(()), dropped)
    }
}

/// Drops `it` inside a catch: the panic, if the drop panics.
fn dropped<T>(it: T) -> Result<(), super::plane::Panic> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(it))).map_err(|payload| super::plane::panicked(&*payload))
}

impl<T> std::ops::Deref for Held<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.it.as_ref().expect("a front end held is there until it is let go")
    }
}

impl<T> std::ops::DerefMut for Held<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.it.as_mut().expect("a front end held is there until it is let go")
    }
}

impl<T> Drop for Held<T> {
    fn drop(&mut self) {
        if let Some(it) = self.it.take()
            && let Err(panic) = dropped(it)
        {
            eprintln!(
                "blaze serve: letting go of {} {panic}; the daemon goes on, the memory it held may be lost",
                self.what
            );
        }
    }
}

/// Unit `unit`'s identity set from the front end, once every run of its closure is applied:
/// from then on nothing changes what its crate is made from.
fn identify(lowered: &mut Lowered, job: &Job, unit: u32) {
    if let Some(slot) = job.identities.get(unit as usize) {
        let _ = slot.set(lowered.identity(unit as usize));
    }
}

/// Every file under the library's `dir`, by its path from there. A link is its target's bytes:
/// the library's digest (`library::digest_of`) was checked when it was found or fetched.
fn library_files(dir: &std::path::Path, prefix: &str, into: &mut Vec<SourceFile>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let full = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", full.display()))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let meta = std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        if meta.is_dir() {
            library_files(&full, &path, into)?;
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable { blaze_vfs::Mode::Executable } else { blaze_vfs::Mode::Regular };
        into.push(SourceFile { path, bytes, mode });
    }
    Ok(())
}

/// Every file under the library's `dir` as [`library_files`] reads it, but looked at only:
/// its path, size, time of its last change and whether it is executable, in name order.
fn library_fingerprint(dir: &std::path::Path, prefix: &str, into: &mut blaze_ids::Hasher) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", entry.path().display()))?;
        entries.push((name.to_owned(), entry.path()));
    }
    blaze_ids::names::sort_by_name(&mut entries, |it| it.0.as_bytes());
    for (name, full) in entries {
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let meta = std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        if meta.is_dir() {
            library_fingerprint(&full, &path, into)?;
            continue;
        }
        let modified = meta.modified().map_err(|error| format!("{}: {error}", full.display()))?;
        // A time before the epoch is hashed as itself, not as the epoch.
        let (before, since) = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(it) => (false, it),
            Err(error) => (true, error.duration()),
        };
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        into.part(path.as_bytes());
        into.update(&meta.len().to_le_bytes())
            .update(&[u8::from(before)])
            .update(&since.as_nanos().to_le_bytes())
            .update(&[u8::from(executable)]);
    }
    Ok(())
}

/// `work` on a worker, the front end's or the build graph's cancel as [`Failed::Again`]: the
/// loop posts it again whole, and nothing of it is kept. Name resolution's typed stops
/// ([`hir_def::nameres::Unmaintained`], [`hir_def::nameres::FixpointLimit`]) become the unit's
/// error. Any other panic is not caught.
pub(crate) fn cancellable<T>(work: impl FnOnce() -> T) -> Result<T, Failed> {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    match caught {
        Ok(it) => Ok(it),
        Err(payload) => {
            if payload.is::<blaze_query::db::Cancelled>() || payload.is::<blaze_graph::Cancelled>() {
                Err(Failed::Again)
            } else if let Some(it) = hir_def::nameres::Unmaintained::of_payload(&*payload) {
                Err(Failed::Error(ApiError::new(it.to_string(), None))) // alloc: an error, once
            } else if let Some(it) = hir_def::nameres::FixpointLimit::of_payload(&*payload) {
                Err(Failed::Error(ApiError::new(it.to_string(), None))) // alloc: an error, once
            } else {
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// One job's front: its units lowered, and what its build scripts and compiles found that
/// later units read.
#[expect(clippy::disallowed_types, reason = "a job's slots, one per unit, each set once by the unit's task: per job, not global")]
pub(crate) struct Job {
    pub(crate) input: JobInput,
    /// Each unit's crate in the front end.
    pub(crate) crates: Arc<Crates>,
    pub(crate) shared: Arc<Shared>,
    /// The unit graph as columns.
    pub(crate) graph: graph::Graph,
    /// Each unit's identity in the front end (what its crate is made from, deeply), set by the
    /// writer once no run left to apply changes it; unset, the unit's record is not known.
    pub(crate) identities: Box<[OnceLock<Option<ContentId>>]>,
    /// What each build script's run told cargo, by its run unit. Each slot is written once,
    /// when the run ends, and read with no lock by every unit after it.
    pub(crate) scripts: Box<[OnceLock<script::Applied>]>,
    /// What each compiled unit made that later work reads: its main file, its tests. Written
    /// once per unit.
    pub(crate) made: Box<[OnceLock<compile::Made>]>,
    /// What each unit's phases made for the phases after them, by unit.
    pub(crate) phases: Box<[Mutex<compile::Phases>]>,
    /// Each unit's record key, worked out once: `fresh` on the loop and `compile` on a worker
    /// both read it.
    pub(crate) keys: Box<[OnceLock<Option<blaze_cas::Key>>]>,
    /// The view and the mounts, as paths.
    pub(crate) mounted: Arc<files::Mounted>,
    /// What each build script prints, by its compile unit, when it provably prints only
    /// constants or is one of std's own (`script::ported`), or why std's can't be said; worked
    /// out once.
    pub(crate) inert: Box<[OnceLock<Result<Option<blaze_usercode::run::Value>, String>>]>,
    /// What the pure path found of each build script, by its compile unit (script::pure), worked out once.
    pub(crate) pure: Box<[OnceLock<script::pure::Purity>]>,
    /// How each unit's program is run, worked out once (the key and the artifact read it).
    pub(crate) runs: Box<[OnceLock<artifact::Runs>]>,
    /// Each build script run prepared by `fresh`, taken by `script`, so it is prepared once.
    /// Only the loop touches it: the lock is never contended.
    pub(crate) starts: Box<[Mutex<Option<Box<script::Start>>>]>,
    /// The crates built for build scripts and proc macros, as the code generator takes them.
    pub(crate) build_time: OnceLock<std::sync::Arc<hir_ty_codegen::v2::BuildTime>>,
    /// The profiles its units' `-C profile-use`s name, read and given to the code generator
    /// once, and held while the job lives.
    pub(crate) profiles: OnceLock<codegen::Profiles>,
    /// What each unit's codegen did with each profile it used, by unit, set once when its
    /// build ends ([`codegen::counted`]); summed over the job, each profile is judged
    /// ([`codegen::judge`]): one that applied to nothing fails the job only when blaze made
    /// it, and is a warning otherwise.
    pub(crate) counted: Box<[OnceLock<Box<[(ContentId, blaze_codegen::driver::ProfileCounts)]>>]>,
    /// The profiles its units' profile lints read (`profile` in `clippy.toml` or
    /// `[workspace.metadata.blaze.lints]`), read once and held while the job lives.
    pub(crate) lint_profiles: OnceLock<compile::LintProfiles>,
    /// The profiles in the target directory's `layout` its images are laid out by, and where
    /// their links write address maps ([`layout`]); read the first time a unit that links
    /// asks, none when there is no such directory.
    pub(crate) layout: OnceLock<Result<Option<layout::Profiles>, String>>,
    /// `blaze test --ub`'s tests, selected once on a worker, read by every evaluation.
    pub(crate) ub: OnceLock<test::UbTests>,
    /// When the job's front was made: the clock the test queue is given.
    pub(crate) began: Instant,
    /// The dependency crates checked this build, kept with the front end until the head's next
    /// build stores their outcomes ([`deps::store`]); one write per such crate.
    later: Mutex<Vec<deps::Later>>,
    /// The trees the job's last edit reparsed in place ([`intent::edit`]), kept with the front
    /// end for the next build.
    seeds: Mutex<Vec<blaze_intent::Seed>>,
    /// The seeds its front end took: the database holds a seed weakly, so it lives as long as
    /// the job.
    _seeded: Vec<blaze_intent::Seed>,
    /// The code generator's memo, taken once at the job's start: every build of the job reads
    /// and puts in it, and no collection runs while the job holds it.
    pub(crate) memo: Arc<blaze_codegen::memo::Memo>,
    /// Every memo entry each unit's build took or put ([`blaze_codegen::driver::Built::memo_used`]),
    /// by unit, set once at its link; held with its front end once it is kept.
    pub(crate) memo_used: Box<[OnceLock<Vec<ContentId>>]>,
    /// What the head's last build used of the memo, kept there until this job's front end is.
    _held: Option<memo::Held>,
}

impl Job {
    fn new(input: JobInput, crates: Arc<Crates>, shared: Arc<Shared>, held: Option<memo::Held>) -> Result<Job, ApiError> {
        let memo = shared.memos.for_job();
        let graph = graph::Graph::new(&input.units)?;
        let config = &input.config;
        let mounted = Arc::new(files::Mounted::new(input.files.clone(), input.root, &config.mounts, &config.invocation.root));
        let slots = graph.len();
        Ok(Job {
            input,
            crates,
            shared,
            graph,
            identities: (0..slots).map(|_| Default::default()).collect(),
            scripts: (0..slots).map(|_| Default::default()).collect(),
            made: (0..slots).map(|_| Default::default()).collect(),
            phases: (0..slots).map(|_| Mutex::default()).collect(),
            keys: (0..slots).map(|_| Default::default()).collect(),
            runs: (0..slots).map(|_| Default::default()).collect(),
            mounted,
            inert: (0..slots).map(|_| Default::default()).collect(),
            pure: (0..slots).map(|_| Default::default()).collect(),
            starts: (0..slots).map(|_| Mutex::new(None)).collect(),
            build_time: Default::default(),
            profiles: Default::default(),
            counted: (0..slots).map(|_| Default::default()).collect(),
            lint_profiles: Default::default(),
            layout: Default::default(),
            ub: Default::default(),
            began: Instant::now(),
            later: Mutex::new(Vec::new()), // alloc: setup, once per job; grows once per dependency crate checked
            seeds: Mutex::new(Vec::new()),
            _seeded: Vec::new(),
            memo,
            memo_used: (0..slots).map(|_| Default::default()).collect(),
            _held: held,
        })
    }

    /// Dependency crate `krate`, checked this build with the lint flags `flags`, whose outcome
    /// `diags` is stored once the front end's images are ([`deps::store`]); `refused` is why
    /// the outcome can't be stored ([`blaze_diagnostics::cache::encode`]), for the build's
    /// report ([`deps::served`]).
    pub(crate) fn check_later(
        &self,
        krate: base_db::Crate,
        flags: Vec<String>,
        diags: Vec<blaze_diagnostics::Diag>,
        refused: Option<String>,
    ) {
        self.later.lock().push(deps::Later { krate, flags, diags, refused });
    }

    pub(crate) fn units(&self) -> &Units {
        &self.input.units
    }

    pub(crate) fn unit(&self, unit: usize) -> Result<&Unit, ApiError> {
        self.units()
            .graph
            .units
            .get(unit)
            .ok_or_else(|| ApiError::new(format!("the build has no unit {unit}"), None))
    }

    pub(crate) fn plan(&self, unit: usize) -> Result<&blaze_units::query::UnitPlan, ApiError> {
        self.units()
            .plans
            .get(unit)
            .ok_or_else(|| ApiError::new(format!("the build has no plan of unit {unit}"), None))
    }

    pub(crate) fn package(&self, unit: &Unit) -> Result<&blaze_units::query::PackagePlan, ApiError> {
        package_of(self.units(), unit)
    }

    /// The unit a diagnostic of unit `at` is of, as cargo's JSON names it, with its index in
    /// the request's units.
    pub(crate) fn unit_of(&self, at: usize) -> Result<UnitOf, ApiError> {
        let unit = self.unit(at)?;
        let package = self.package(unit)?;
        let id = u32::try_from(at)
            .map_err(|_| ApiError::new(format!("unit {at} is past the 2^32 units a request can name"), None))?;
        Ok(UnitOf {
            id,
            test: matches!(unit.mode, Mode::Test) || (matches!(unit.mode, Mode::Check) && unit.checked_as_test),
            package_id: unit.pkg_id.clone(),
            manifest_path: package.manifest_path.clone(),
            target: unit_target(unit)?,
        })
    }

    /// `BLAZE_SANDBOX=off` in the request's environment: every build script and test of the
    /// command runs outside the sandbox (docs/cargo.md).
    pub(crate) fn sandbox_off(&self) -> bool {
        sandbox_off(&self.input.config.invocation.env)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.input.cancelled.load(Ordering::Relaxed)
    }
}

pub(crate) fn sandbox_off(env: &[(String, String)]) -> bool {
    env.iter().any(|(name, value)| name == "BLAZE_SANDBOX" && value == "off")
}

pub(crate) fn package_of<'a>(
    units: &'a Units,
    unit: &Unit,
) -> Result<&'a blaze_units::query::PackagePlan, ApiError> {
    units
        .packages
        .binary_search_by(|it| it.id.as_str().cmp(&unit.pkg_id))
        .ok()
        .map(|at| &units.packages[at])
        .ok_or_else(|| ApiError::new(format!("the build has no package `{}`", unit.pkg_id), None))
}

/// A unit's target, as cargo's JSON messages give it. Its kind is cargo's first word for it
/// (any word but `bin`, `test`, `bench`, `example` and `custom-build` is a library's crate
/// type); a crate type blaze doesn't know is an error naming it, as rustc refuses it.
pub(crate) fn unit_target(unit: &Unit) -> Result<UnitTarget, ApiError> {
    let target = &unit.target;
    let Some(first) = target.kind.first() else {
        return Err(ApiError::new(format!("target `{}` has no kind in cargo's unit graph", target.name), None));
    };
    let kind = TargetKind::named(first).unwrap_or(TargetKind::Lib);
    let crate_types = target
        .crate_types
        .iter()
        .map(|word| {
            CrateType::named(word).ok_or_else(|| {
                // alloc: error path
                ApiError::new(format!("target `{}` has the crate type `{word}`, which rustc doesn't know", target.name), None)
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(UnitTarget {
        kind,
        crate_types,
        name: target.name.clone(),
        src_path: target.src_path.to_string_lossy().into_owned(),
        edition: target.edition.clone(),
        doc: target.doc,
        doctest: target.doctest,
        test: target.test,
    })
}

/// Whether the job stops `unit` after the front end: a check, or every unit of `cargo check`
/// but a build script's compile, which is linked as cargo links it there, for its run.
pub(crate) fn checks_only(goal: Goal, unit: &Unit) -> bool {
    match unit.mode {
        Mode::Check => true,
        Mode::Build if unit.target.is_custom_build() => false,
        _ => goal == Goal::Check,
    }
}

/// The phases of `unit`'s compile, each a node of the plane ([`Phase`]): no codegen and no
/// link for a unit [`checks_only`] stops after the front end.
pub(crate) fn phases(goal: Goal, unit: &Unit) -> &'static [Phase] {
    match checks_only(goal, unit) {
        true => &Phase::CHECK,
        false => &Phase::BUILD,
    }
}

/// The key phase `phase` (its codegen or its link) of a program's build is predicted and
/// taught by (`plane::Plane::push_program`), when `unit` builds a program its client runs: a
/// binary, test, bench, example or `cdylib`. A build script and a proc macro are not: many
/// and small, they are admitted as any compile. The same program of the same profile and
/// platform has the same key in every build and daemon.
pub(crate) fn program_key(unit: &Unit, phase: Phase) -> Option<blaze_graph::Key> {
    // A wasm proc macro's units run in the macro host, never as a program of the client's.
    if unit.target.is_custom_build() || unit.macro_wasm || !matches!(unit.mode, Mode::Build | Mode::Test) {
        return None;
    }
    if !matches!(codegen::makes(unit), Ok(codegen::Makes::Executable | codegen::Makes::Shared { proc_macro: false })) {
        return None;
    }
    let mut hasher = blaze_ids::Hasher::new("blaze v2 program build 2");
    let platform = unit.platform.as_deref().unwrap_or("");
    let parts = [unit.pkg_id.as_str(), &unit.target.name, &unit.profile.name, platform];
    for part in parts.into_iter().chain(unit.target.kind.iter().map(String::as_str)) {
        hasher.update(&(part.len() as u64).to_le_bytes()).update(part.as_bytes());
    }
    hasher.update(&[unit.mode as u8, phase as u8]);
    Some(blaze_graph::Key(hasher.finish().0))
}

/// A job's front as the loop holds it.
pub struct JobFront(pub(crate) Arc<Job>);

impl Front<Real> for JobFront {
    fn fresh(&self, unit: usize) -> bool {
        match self.0.unit(unit).map(|it| it.mode) {
            Ok(Mode::RunCustomBuild) => script::fresh(&self.0, unit),
            Ok(_) => compile::fresh(&self.0, unit),
            Err(_) => false,
        }
    }

    fn compile(&self, reader: UnitReader, unit: usize, phase: Phase, images: &mut Vec<Image<Real>>) -> Result<Option<Compiled>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| compile::compile(&self.0, &reader, unit, phase, images))?
    }

    fn refused(&self, unit: usize, what: String) -> blaze_api::Diagnostic {
        say::refused(what, self.0.unit_of(unit).ok(), self.0.input.color)
    }

    fn script(&self, unit: usize, tags: u64) -> Result<UnitRuns, ApiError> {
        script::start(&self.0, unit, tags).map(UnitRuns::Script)
    }

    fn tests(&self, built: &[Artifact], selection: Selection<'_>, tags: u64) -> Result<UnitRuns, ApiError> {
        test::start(&self.0, built, selection, tags).map(UnitRuns::Tests)
    }

    fn ub_tests(&self, reader: UnitReader, selection: Selection<'_>) -> Result<Vec<String>, Failed> {
        cancellable(|| test::ub_tests(&self.0, &reader, selection))?
    }

    fn evaluate(&self, reader: UnitReader, test: &str, ub: Ub) -> Result<TestResult, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| test::evaluate(&self.0, reader.db(), test, ub))?
    }

    fn run(
        &self,
        built: &[Artifact],
        bin: Option<&str>,
        args: &[String],
        tags: u64,
    ) -> Result<UnitRuns, ApiError> {
        run::start(&self.0, built, bin, args, tags).map(UnitRuns::Program)
    }

    fn reach(
        &self,
        after: UnitReader,
        from: &JobFront,
        before: UnitReader,
        changes: &[blaze_vfs::Change],
    ) -> Result<Reach, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| reach::reach(&from.0, before.db(), &self.0, after.db(), changes))?
    }

    fn edit(&self, reader: UnitReader, edits: &[blaze_api::Edit]) -> Result<blaze_api::Edited, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| intent::edit(&self.0, &reader, edits))?
    }

    fn ahead(&self, reader: UnitReader, filter: &[String]) -> Result<Vec<u32>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| intent::ahead(&self.0, &reader, filter))?
    }

    fn codegen_inputs(&self, reader: UnitReader) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        // Read from the units' records in the store: the front end is not asked.
        drop(reader);
        cancellable(|| compile::codegen_inputs(&self.0))?
    }

    fn tables(&self, mut writer: UnitWriter, images: &[(&'static str, u64, u64)]) -> Result<Vec<Table>, Failed> {
        let mut counted = writer.tables()?;
        counted.extend_from_slice(images);
        Ok(summed(counted))
    }

    fn pack(&self, mut writer: UnitWriter, program: &str) -> Result<Vec<u8>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        if !writer.apply()? {
            let what = "the pack is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        }
        let Some(lowered) = Arc::get_mut(&mut writer.front) else {
            let what = "the pack is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| pack::pack(&self.0, lowered, program))?.map_err(Failed::Error)
    }

    fn doc(&self, mut writer: UnitWriter, no_deps: bool, private: bool) -> Result<Vec<Response>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        if !writer.apply()? {
            let what = "the documentation is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        }
        let Some(lowered) = Arc::get_mut(&mut writer.front) else {
            let what = "the documentation is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| doc::doc(&self.0, lowered, no_deps, private))?.map_err(Failed::Error)
    }
}

impl Kept for codegen::Linked {
    fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        codegen::Linked::tables(self)
    }
}

/// User code of a job, run on the loop: a build script's run, the build's tests, or its
/// program.
pub enum UnitRuns {
    Script(script::ScriptRun),
    Tests(test::TestRun),
    Program(run::Program),
}

impl Runs<Real> for UnitRuns {
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.start(store, writer, effects, out),
            UnitRuns::Tests(it) => it.start(store, writer, effects, out),
            UnitRuns::Program(it) => it.start(store, writer, effects, out),
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        match self {
            UnitRuns::Script(it) => it.owns(tag),
            UnitRuns::Tests(it) => it.owns(tag),
            UnitRuns::Program(it) => it.owns(tag),
        }
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.complete(completion, store, writer, effects, out),
            UnitRuns::Tests(it) => it.complete(completion, store, writer, effects, out),
            UnitRuns::Program(it) => it.complete(completion, store, writer, effects, out),
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.cancel(effects),
            UnitRuns::Tests(it) => it.cancel(effects),
            UnitRuns::Program(it) => it.cancel(effects),
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        match self {
            UnitRuns::Script(it) => it.take_puts(),
            UnitRuns::Tests(it) => it.take_puts(),
            UnitRuns::Program(it) => it.take_puts(),
        }
    }
}

/// The front end's tables (tokens, syntax nodes, HIR expressions, MIR statements, memos and
/// their edges, each as its owner counts it) and the linker's (relocations and the rest),
/// summed by name and sorted by it.
fn summed(mut counted: Vec<(&'static str, u64, u64)>) -> Vec<Table> {
    counted.sort_unstable_by_key(|it| it.0);
    let mut tables: Vec<Table> = Vec::new();
    for (name, elements, bytes) in counted {
        match tables.last_mut() {
            Some(table) if table.name == name => {
                table.elements += elements;
                table.bytes += bytes;
            }
            _ => tables.push(Table { name: name.to_owned(), elements, bytes }),
        }
    }
    tables
}

/// A warning of the whole job, of no unit or span: `message` as the job says it.
fn warning(message: String) -> blaze_api::Diagnostic {
    blaze_api::Diagnostic {
        level: blaze_api::Level::Warning,
        code: None,
        rendered: format!("warning: {message}\n"), // alloc: once per warning
        message,
        spans: Vec::new(),    // alloc: none
        children: Vec::new(), // alloc: none
        facts: Vec::new(),    // alloc: none
        unit: None,
        explanation: None,
    }
}
