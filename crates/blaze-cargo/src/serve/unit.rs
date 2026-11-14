//! The daemon's real build ([`Real`], the [`Steps`] of `blaze serve`; docs/v2/plan.md
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
//! - [`say`]: cargo's own lines (`error: ...`, `warning: ...`) as diagnostics.
//! - [`BuildEnd`]: what a build's end lets go of its front end.
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
/// The store's keys of an output's bytes, for the request certificate's look at an output
/// (`serve::job::certify`).
pub(super) use compile::{linked_key, output_key};
mod configs;
mod deps;
mod doc;
mod drawn;
mod error;
pub(crate) mod files;
mod graph;
mod harness;
mod intent;
mod layout;
mod memo;
mod perf;
mod prove;
// Lane packs-1112: `Places` is also the export's guard against a record that holds a
// directory of this machine (`serve::packs::home_places`).
pub(crate) mod portable;
mod reach;
mod reads;
pub(crate) mod run;
mod say;
mod script;
pub(crate) use compile::{Filed, PendingRecord, file_records, span_unit};
pub(crate) use script::compile_of;
mod test;

#[cfg(test)]
mod library_tests;
#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

#[expect(
    clippy::disallowed_types,
    reason = "a job's slots, one per unit, each set once by the unit's task: per job, not global"
)]
use std::sync::OnceLock;

use base_db::{FileId, Trigger};
use blaze_api::{
    ApiError, Artifact, CrateType, Reach, Response, Table, TargetKind, TestResult, Ub, UnitOf,
    UnitTarget,
};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, Tag, lock::Lock};
use blaze_ids::ContentId;
use blaze_lower::{Crates, Db, Lower, Lowered, ProcMacroLoadResult, Ran};
use blaze_units::{
    query::{BuildConfig, EnvMoved, EnvReads, Fetch, Units},
    schema::{Mode, Unit},
};

use crate::serve::{
    Paths,
    fetch::Fetcher,
    work::{
        Asks, Broke, Compiled, Failed, Front, FrontEnd, Image, JobInput, JobKind, Kept, KeptDb,
        KeptPut, Library, Phase, Put, RunPuts, Runs, Selection, Source, SourceFile, Steps,
        WriteWait, Writer, applying, applying_fallible,
    },
};

/// The build `blaze serve` runs: one for the daemon's life, shared by every job.
pub struct Real {
    fetcher: Fetcher<blaze_registry::http::HyperHttp>,
    shared: Arc<Shared>,
    /// blaze's sources directory ([`blaze_manifest::home::sources`]), the one place a fetched
    /// package is written on disk ([`crate::serve::fetch::keep_on_disk`]).
    sources: PathBuf,
    /// Where the pinned native inputs are laid out, `<BLAZE_HOME>/native`
    /// ([`crate::serve::native`]); none when the daemon has no home (neither `BLAZE_HOME` nor
    /// the user's home is set), which only a build that names an input is refused for.
    native: Option<PathBuf>,
    /// The layouts this daemon has checked against their marks, each once
    /// ([`crate::serve::native::Verified`]).
    native_verified: crate::serve::native::Verified,
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
    /// blaze's sources directory ([`blaze_manifest::home::sources`]): a root of every
    /// build's places ([`portable::Places`]), so a registry crate's keys and front-end
    /// records do not say which home fetched it.
    pub(crate) sources: PathBuf,
    /// The code generator's memo, which each job takes at its start ([`Job::memo`]).
    pub(crate) memos: Arc<memo::Memos>,
    /// What `-C target-cpu=native` is on this machine; none without LLVM.
    pub(crate) host_cpu: Option<blaze_units::stdlib::target::HostCpu>,
    /// Test selection's one selector per suite, for the daemon's life
    /// (fastcompile-sol-select 1): each test run takes its suites' and gives them back.
    pub(crate) selectors: test::image::Selectors,
    /// The dev executables in the daemon's code memory, by path, which a `run` starts from
    /// and each link patches (docs/v2/plan/design/compile-time.md).
    pub(crate) dev_runs: run::inproc::DevRuns,
    /// The programs blaze's launcher runs, and the socket their launchers connect to
    /// (docs/v2/plan/design/launcher.md).
    pub(crate) launches: run::launch::Launches,
}

impl Real {
    pub fn new(paths: &Paths, sources: PathBuf) -> Result<Real, String> {
        let fetcher = Fetcher::new(paths.state.join("fetch"))?;
        let tools = paths.state.join("tools");
        stand_ins(&tools, &paths.launcher)?;
        let shared = Shared {
            state: paths.state.clone(),
            launcher: paths.launcher.clone(),
            tools,
            // clone: once a daemon
            sources: sources.clone(),
            memos: memo::Memos::new(),
            host_cpu: hir_ty_codegen::host_cpu().map(|(name, features)| {
                blaze_units::stdlib::target::HostCpu {
                    arch: std::env::consts::ARCH.to_owned(),
                    name,
                    features,
                }
            }),
            selectors: test::image::Selectors::default(),
            dev_runs: run::inproc::DevRuns::new(paths.state.join("inproc")),
            launches: run::launch::Launches::new(paths.state.join("launch")),
        };
        let native = blaze_manifest::user_config::home().ok().map(|it| it.join(crate::serve::native::NATIVE));
        Ok(Real {
            fetcher,
            shared: Arc::new(shared),
            sources,
            native,
            native_verified: Default::default(),
        })
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
            Ok(_) => {
                std::fs::remove_file(&at).map_err(|error| format!("{}: {error}", at.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", at.display())),
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(launcher, &at)
            .map_err(|error| format!("{}: {error}", at.display()))?;
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
    type Proving = prove::Proving;

    fn check_proofs(proving: prove::Proving, lean: Option<&str>, tags: u64) -> UnitRuns {
        UnitRuns::Proofs(prove::Checking::new(proving, lean, tags))
    }

    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        let source = self.fetcher.fetch(fetch)?;
        crate::serve::fetch::keep_on_disk(fetch, &source, &self.sources)?;
        Ok(source)
    }

    fn native(
        &self,
        input: &blaze_units::query::NativeInput,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<std::path::PathBuf, ApiError> {
        let Some(native) = &self.native else {
            return Err(ApiError::new(
                blaze_report::say!(
                    "pinned native input ",
                    blaze_report::say::Code(&input.name),
                    " is laid out under blaze's home, and the daemon has none: set BLAZE_HOME (or HOME) where it starts"
                )
                .flat(),
                Some(input.pin.clone()), // clone: an error's place, once
            ));
        };
        let fetcher = self.fetcher.pinned();
        crate::serve::native::ensure(input, native, &fetcher, &self.native_verified, cancelled)
    }

    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError> {
        let at = || Some("the standard library's sources".to_owned());
        let library =
            crate::toolchain::library::fetch().map_err(|what| ApiError::new(what, at()))?;
        let named = blaze_units::stdlib::library_at(&library.commit);
        if crate::toolchain::library::owned(&library).is_some() {
            return library_from_texts(&library.dir, &library.digest, named, kept);
        }
        let in_dir = |what| ApiError::new(what, Some(library.dir.display().to_string()));
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

    fn launch(
        &self,
        program: &str,
        pid: u32,
        placed: Option<[u8; 32]>,
    ) -> Result<Option<Vec<(String, String)>>, ApiError> {
        self.shared
            .dev_runs
            .launch(program, pid, placed)
            .map_err(|what| ApiError::new(what, Some(program.to_owned())))
    }

    fn held_code(
        &self,
        holder: crate::serve::work::CodeHolder,
        program: &str,
    ) -> Option<crate::serve::work::HeldCode> {
        match holder {
            crate::serve::work::CodeHolder::Launcher => self.shared.launches.held_code(program),
            crate::serve::work::CodeHolder::Memory => self.shared.dev_runs.held_code(program),
        }
    }

    fn lease(
        &self,
        units: &blaze_units::query::Units,
        checkout: &str,
        builds: &str,
    ) -> crate::serve::work::Leased {
        crate::serve::job::lease_build_dirs(units, checkout, builds)
    }

    fn store_kept(&self, kept: &mut KeptLowered) -> Result<Vec<KeptPut>, ApiError> {
        // Its parts, borrowed apart below.
        let kept: &mut KeptParts = kept;
        // The build's end stores the sealed crates and keeps what it was served with; the
        // second store, at a stop (`super::job::fronts`'s `store_members`), writes the members and the crates above
        // them, and whatever was refused before is tried once more. An edited member's pages
        // written at the build's end would be paid by the next edit and thrown away by it.
        let (which, later, deps) = match kept.stored {
            Stored::All => return Ok(Vec::new()), // alloc: none
            Stored::No => {
                // The outcomes of the sealed crates now; a member's waits for its image,
                // which the second store writes.
                let db = kept.lowered.db();
                // alloc: setup: once per front end stored, one entry a crate walked
                let mut sealed = blaze_hash::HashMap::new_in(std::alloc::Global);
                // alloc: once per front end stored, the outcomes it checked split in two
                let (now, members): (Vec<deps::Later>, Vec<deps::Later>) =
                    std::mem::take(&mut kept.later)
                        .into_iter()
                        .partition(|it| hir_def::deps::sealed_in(db, it.krate, &mut sealed));
                kept.later = members;
                (hir_def::deps::Which::Sealed, now, None)
            }
            Stored::Sealed => {
                (hir_def::deps::Which::All, std::mem::take(&mut kept.later), kept.deps.take())
            }
        };
        // What it was served with stays with it for the second store until that store takes it.
        let served = match which {
            hir_def::deps::Which::Sealed => kept.deps.as_ref(),
            hir_def::deps::Which::All => deps.as_ref(),
        };
        let stored = cancellable(|| {
            deps::store(&kept.lowered, served, &mut kept.session, &later, &kept.root, which)
        });
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
        kept.stored = match which {
            hir_def::deps::Which::Sealed => Stored::Sealed,
            hir_def::deps::Which::All => Stored::All,
        };
        // What storing ran (each dependency's signatures) is folded into the reverse edges
        // here, after the answer and with the front end exclusive, never at the next build's
        // first write (`Storage::pack_reverse`).
        blaze_query::db::Database::storage_mut(kept.lowered.served_db()).pack_reverse();
        // And settled: what those runs said changed leaves the next walk's start
        // (`Storage::settle_changed_set`).
        blaze_query::db::Database::storage_mut(kept.lowered.served_db()).settle_changed_set();
        Ok(puts)
    }

    fn kept_refusal(&self, kept: &mut KeptLowered) -> Option<String> {
        kept.flushed.as_mut().and_then(deps::Flushes::take_refusal)
    }

    fn kept_run_log(&self, kept: &KeptLowered) -> blaze_query::db::Taken {
        hir_def::span_names::take_run_log(kept.lowered.db())
    }

    fn serve_needs(&self, kept: &mut KeptLowered, wanted: &std::sync::atomic::AtomicBool) -> Vec<KeptPut> {
        run::launch::serve_needs(kept, &self.shared.launches, wanted)
            .into_iter()
            .map(|it| KeptPut {
                put: Put { key: it.key, kind: it.kind, bytes: it.bytes },
                what: Arc::from("a launched run's first call"),
                into: super::work::PutInto::Home,
            })
            .collect()
    }

    fn wake_needs(&self) {
        self.shared.launches.wake();
    }

    fn launch_marked(
        &self,
        store: blaze_cas::Reader,
        waker: Option<blaze_core::Waker>,
        marked: Vec<super::work::LaunchMarked>,
    ) {
        self.shared.launches.marked(store, waker, marked);
    }

    fn launch_kept(&self) -> Vec<super::work::LaunchKept> {
        self.shared.launches.kept()
    }

    fn launch_wants(&self) -> Vec<String> {
        self.shared.launches.wants()
    }

    fn launchers_connected(&self) -> usize {
        self.shared.launches.connected()
    }

    fn launch_execd(&self, pid: u32, build: u64, exec_ns: i64) {
        self.shared.launches.execd(pid, build, exec_ns);
    }

    fn launch_spans(&self) -> Vec<crate::serve::unit::run::launch::ProgramSpans> {
        self.shared.launches.program_spans()
    }

    fn launch_built(&self, program: &str, built: Result<(), String>) {
        self.shared.launches.built_by_daemon(program, built);
    }

    fn release_kept(&self, kept: &mut KeptLowered, wanted: &std::sync::atomic::AtomicBool) {
        let Some(end) = kept.end.take() else { return };
        // A request of the checkout waits for this front end: it goes back now, and that
        // build's own end lets go of what this one would have.
        if wanted.load(Ordering::Acquire) {
            tracing::info_span!(
                target: "blaze",
                "front end release left to the checkout's next build, which waits for the front end",
                checkout = %kept.root,
            )
            .in_scope(|| {});
            return;
        }
        end.release(&mut kept.lowered);
    }

    fn lower(&self, input: &JobInput, kept: Option<KeptLowered>) -> Result<FrontEnd<Real>, Failed> {
        if input.cancelled.load(Ordering::Relaxed) {
            return Err(Failed::Again);
        }
        let writer = cancellable(
            move || -> Result<(UnitWriter, Vec<Put>, Vec<blaze_kernel::Miss>), Failed> {
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
                    if it.stored == Stored::No {
                        let what = "the head's last front end is lowered into, and what its build left for the store was never written (`Steps::store_kept`)";
                        return Err(Failed::Error(ApiError::new(what, Some(it.root.clone())))); // clone: an error, once
                    }
                    flushed = it.flushed.take();
                }
                // Whether a record retracting a value verified across checkouts was published since
                // the kept front end was lowered (rule 21): its database's tracked expansions and
                // served images would keep such a value, so it is not lowered into, and the build
                // lowers afresh, where `deps::serve` works every verified image's reads out again
                // and an expansion is looked up again. Read before this job expands anything, so a
                // record published while it runs moves the epoch past the one its front end is kept
                // at. One atomic load (`Keeping::retractions_now`).
                let retractions = input.macros.keeps().retractions_now();
                // A kept front end that is not lowered into is let go by name: the build lowers
                // anew, and the daemon's log and the profile say which part of its inputs moved.
                let mut misses = Vec::new(); // alloc: none unless a kept front end is let go for a variable
                let kept = match kept {
                    Some(it) => match unkept(&it, input, retractions) {
                        None => Some(it),
                        Some((why, moved)) => {
                            if let Some(moved) = &moved {
                                misses.push(env_miss(&it, moved));
                            }
                            let_go(it, &why, moved.as_ref().map(|it| it.name.as_str()));
                            None
                        }
                    },
                    None => None,
                };
                // What pins each package, by the one rule of what is sealed
                // (`serve::sealed::pin`): the planner writes it on each crate.
                let sealed = compile::pins(&input.units, &input.config.sources).map_err(Failed::Error)?;
                let lower = || Lower {
                    units: &input.units,
                    config: &input.config,
                    files: &input.files,
                    root: input.root,
                    host_cpu: self.shared.host_cpu.as_ref(),
                    sealed: &sealed,
                };
                let failed = |error| Failed::Error(lower_error(error));
                // A front end kept and moved to the view keeps what it was served, until a serve of
                // this job finds a crate's key moved: moving reads again only the view's files, and
                // the runs and proc macros applied again are served after as in a fresh job.
                //
                // A request of another plan (`test` after `build`, `--release`, another feature
                // set) is lowered into the same front end: its units are a view over the crate
                // graph the front end keeps (`Lowered::relower`), each crate found or made
                // beside the others, so the switch costs the crates the plan adds and a plan
                // seen before costs nothing. Until 2026-11-12 the plan's units were the front
                // end's identity (`unkept`), and every switch lowered the workspace again, the
                // standard library included (hello-world: 4.5 s a switch, 820,026 queries).
                let (mut lowered, session, seeded, walks) = match kept.map(KeptLowered::into_parts) {
                    Some(KeptParts { walks, session, mut lowered, seeds, units: kept_units, .. }) => {
                        let same_plan =
                            Arc::ptr_eq(&kept_units, &input.units) || kept_units == input.units;
                        // Why the front end can't take this request, when it can't.
                        let moved: Option<String> =
                            match lowered.refresh(lower(), &input.looks, |db| seed(db, &seeds)).map_err(failed)? {
                                // alloc: a reason, once per front end let go
                                false => Some("the view moves the workspace's root or what its root manifest sets, which every crate is made with, or whether a crate of the macro host links proc_macro, which its cfg is made with".to_owned()),
                                true if same_plan => None,
                                true => {
                                    let span = tracing::info_span!(
                                        target: "blaze",
                                        "plan lowered into the kept front end",
                                        checkout = input.config.invocation.root.as_str(),
                                        crates_made = tracing::field::Empty,
                                        roots_made = tracing::field::Empty,
                                        dirs_read = tracing::field::Empty,
                                    );
                                    // clone: the span's handle, so what the plan made is written in it
                                    let _span = span.clone().entered();
                                    match lowered.relower(lower()).map_err(failed)? {
                                        blaze_lower::Relowered::Into(placed) => {
                                            span.record("crates_made", placed.crates_made as u64);
                                            span.record("roots_made", placed.roots_made as u64);
                                            span.record("dirs_read", placed.dirs_read as u64);
                                            None
                                        }
                                        // alloc: a reason, once per front end let go
                                        blaze_lower::Relowered::Anew(why) => Some(format!(
                                            "the request's plan can't be lowered beside the plans it holds: {why}"
                                        )),
                                    }
                                }
                            };
                        match moved {
                            None => (lowered.into_inner(), Some(session), seeds, Some(walks)),
                            Some(why) => {
                                let why = why.as_str();
                                let checkout = input.config.invocation.root.as_str();
                                {
                                    let variable = "";
                                    let _span =
                                tracing::info_span!(target: "blaze", "kept front end let go", checkout, why, variable).entered();
                                    crate::serve::said::logged!(WARN,
                                        "blaze serve: the kept front end of {checkout} is let go, and its build lowers anew: {why}"
                                    );
                                    // The walks first: they name the database's ids.
                                    drop((walks, session, lowered, seeds));
                                }
                                (blaze_lower::lower(lower()).map_err(failed)?, None, Vec::new(), None)
                            }
                        }
                    }
                    None => (blaze_lower::lower(lower()).map_err(failed)?, None, Vec::new(), None),
                };
                // What codegen's walks over this database keep for its next build: the kept
                // front end's, while its database is the one refreshed into; a new database
                // (lowered anew, or made again by a refresh) begins with none.
                let walks = match walks {
                    Some(walks) if walks.is_of(lowered.db()) => walks,
                    // alloc: once per database lowered, its walks' owner
                    _ => Arc::new(hir_ty_codegen::v2::Walks::new(lowered.db())),
                };
                // How the front end's identities write paths, told before any is asked: against
                // the build's directories ([`portable::Places`]), so a unit's record key does
                // not say where the checkout, its build directory or the machine's are.
                lowered.set_port(std::sync::Arc::new(portable::Places::new(
                    &input.config.invocation.root,
                    &input.units.target_dir,
                    &input.config.std,
                    &self.shared.tools,
                    &self.shared.launcher,
                    &self.shared.sources,
                ).port()));
                // Each crate's surface, which a crate above it enters its image key by
                // (`hir_def::deps::key`), given now that the port is told and before the
                // first key is asked: the lowering's own writes give it from here on. A
                // front end kept from the last build holds them already, and nothing is
                // written.
                //
                // A database lowered anew reads the found record of each crate whose plan key
                // it has first (restore.md, "Restart reads records (d)", C): what its image is
                // filed under, and its own surface, which `give_surfaces` then takes in place
                // of parsing the crate's files. A kept front end found them when it was made.
                if session.is_none() {
                    deps::find(&mut lowered, &input.store)?;
                }
                lowered.give_surfaces();
                                // Before any key is resolved: two crates of one key would take each other's items.
                blaze_items::key::check_crate_keys(lowered.db()).map_err(|clash| {
                    Failed::Error(ApiError::new(format!("blaze-items: {clash}"), None))
                })?; // alloc: an error, once
                let fresh = session.is_none();
                if fresh {
                    blaze_item_hash::impls::install(lowered.served_db());
                    // Which front end computes a body's identity, told before any is asked:
                    // the one the goal store and the item store are given below. Until
                    // 2026-11-10 nothing told it, so every identity was `Err("no front-end
                    // build is set")` behind an untracked read of the singleton's absence:
                    // the item store refused every body (`Refusal::NoIdentity`), and
                    // `identity_of` ran again for every body it was asked of in every
                    // revision (about 10,000 runs a build, all `ran_untracked`). A database
                    // lowered into again was told when it was made.
                    blaze_item_hash::identity::set_build(lowered.served_db(), deps::front_end()?);
                }
                // The guard: a build whose database was not told stops here, naming it, where
                // it used to go on with no identity and an untracked read a body.
                if blaze_item_hash::identity::FrontEnd::try_get(lowered.db()).is_none() {
                    // alloc: an error, once
                    return Err(Failed::Error(ApiError::new(
                        "the front end's database was not told which front end computes its bodies' identities (`blaze_item_hash::identity::set_build`), so each would be asked again in every revision and none filed",
                        None,
                    )));
                }
                let mut session = match session {
                    Some(session) => session,
                    None => deps::session(
                        &lowered,
                        &input.store,
                        &input.reads,
                        &input.macros,
                        &input.config.invocation.env,
                    )?,
                };
                if fresh {
                    // Where the database keeps its per-item entries, told before the first
                    // serve below reads a crate from them.
                    deps::entries(&mut lowered, &session);
                }
                let mut job =
                    Job::new(input.clone(), lowered.crates().clone(), self.shared.clone(), held)?;
                job._seeded = seeded;
                let job = Arc::new(job);
                let waits = graph::Waits::new(&job.graph);
                // After the seeds (the root's parse is seeded as on disk), before any identity:
                // a test crate is identified by its root with the harness.
                // A unit refused here has no write to charge it to yet: the lowering fails, naming each.
                harness::write(
                    &mut lowered,
                    &job,
                    (0..job.graph.len() as u32).filter(|&unit| waits.ready(unit)),
                )
                .map_err(|refused| harness::refused(&refused))?;
                for unit in 0..job.graph.len() as u32 {
                    if unit % 64 == 0 && input.cancelled.load(Ordering::Relaxed) {
                        return Err(Failed::Again);
                    }
                    if waits.ready(unit) {
                        identify(&mut lowered, &job, unit)?;
                    }
                }
                // Kept or fresh alike: the session's keys say which crates' keys moved, and a kept
                // front end's serve of the units ready now finds none moved unless a write before
                // this job's moved one.
                let mut deps = None;
                deps::serve(
                    &mut lowered,
                    &job,
                    &deps::ready(&job, &waits),
                    &mut session,
                    &mut deps,
                )?;
                let end = BuildEnd::new(lowered.crates(), job.graph.len(), input.kind != JobKind::Check);
                let writer = UnitWriter {
                    walks,
                    session,
                    front: Held::new(Arc::new(lowered), "a job's front end"),
                    job,
                    waits,
                    ready: Vec::new(),
                    runs: Vec::new(),
                    macros: Vec::new(),
                    msls: Vec::new(),
                    deps,
                    flushed,
                    broke: Vec::new(),
                    broken: false,
                    retractions,
                    end,
                };
                Ok((writer, puts, misses))
            },
        )??;
        let (writer, puts, misses) = writer;
        let front = Arc::new(JobFront(writer.job.clone()));
        Ok(FrontEnd { front, writer, puts, misses })
    }
}

/// Serves the crates of `written`, the units a batch of runs and loads made ready or a loaded
/// proc macro reaches ([`deps::serve`]), adding to what `kept` holds of the job's serves:
/// after the harness's roots and the identities, for a key those writes moved. A serve that
/// fails or panics has left the front end partly served, which no unit's write owns: the
/// error is the writer's own, naming the units served; no unit is built over it.
fn serve_written(
    lowered: &mut Lowered,
    job: &Job,
    written: &[u32],
    session: &mut deps::Session,
    kept: &mut Option<deps::Kept>,
) -> Result<(), ApiError> {
    let Some(&first) = written.first() else { return Ok(()) };
    let served = applying(first as usize, "serving dependencies", || {
        deps::serve(lowered, job, written, session, kept)
    });
    let error = match served {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error,
        Err(broke) => ApiError::new(broke.how.to_string(), None), // alloc: an error, once
    };
    let units =
        written.iter().map(|&it| lowered.what(it as usize)).collect::<Vec<_>>().join(", "); // alloc: an error, once
    let at = error.at.as_deref().map(|at| format!(" at {at}")).unwrap_or_default(); // alloc: an error, once
    let what = format!(
        "serving from the store the dependencies of {units}, which a batch of build scripts' runs and \
         proc macros' loads made ready, failed{at}: {}; the front end is left partly served, and no unit \
         is built over it",
        error.what
    ); // alloc: an error, once
    Err(ApiError::new(what, error.at))
}

/// Why job `input`'s lowering does not lower into the kept front end `kept`, by the part of
/// its inputs that moved, with the variable when that part is one the front end read of the
/// environment ([`env_moved`]); `None` when it does. `retractions` is the epoch read now
/// ([`proc_macro_api::kept::Keeping::retractions_now`]).
///
/// The request's plan is no part of it: a plan is a view over the front end's crate graph,
/// lowered into it ([`blaze_lower::Lowered::relower`]). The control of that is
/// [`PLAN_IS_IDENTITY`].
fn unkept(
    kept: &KeptLowered,
    input: &JobInput,
    retractions: u64,
) -> Option<(String, Option<EnvMoved>)> {
    if plan_is_identity(input)
        && !Arc::ptr_eq(&kept.units, &input.units)
        && kept.units != input.units
    {
        // alloc: a reason, once per front end let go
        return Some((
            format!(
                "the plan's units moved ({}), and `{PLAN_IS_IDENTITY}=1` makes the plan the front end's identity",
                units_differ(&kept.units, &input.units)
            ),
            None,
        ));
    }
    if let Some(part) = blaze_lower::config_differs(&kept.config, &input.config) {
        // alloc: a reason, once per front end let go
        return Some((format!("its build config's {part} moved"), None));
    }
    if let Some(moved) =
        env_moved(&kept.lowered, kept.session.env_read(), &input.config.invocation.env)
    {
        // alloc: a reason, once per front end let go
        return Some((
            format!("{moved} (rule 21: a front end is keyed by the variables it read)"),
            Some(moved),
        ));
    }
    if kept.retractions != retractions {
        // alloc: a reason, once per front end let go
        return Some((
            format!(
                "a record retracting a value verified across checkouts was published since it was lowered \
                 (retractions epoch {} then, {retractions} now; rule 21)",
                kept.retractions
            ),
            None,
        ));
    }
    None
}

/// The variable of a request that plants the bug the kept front end's tests are held to
/// (test-the-cause): set to `1`, the plan's units are the front end's identity again, as they
/// were until 2026-11-12, so a request of another plan lets the front end go and lowers the
/// workspace anew. The revision case `a_switch_of_request_with_the_plan_as_identity` sets it
/// and must then miss the bounds `switches_of_request_are_no_ops` holds. Never set in use: it
/// is read of the request alone, and changes what is kept, never what is built.
const PLAN_IS_IDENTITY: &str = "BLAZE_FRONT_END_PER_PLAN";

/// Whether `input`'s request sets [`PLAN_IS_IDENTITY`].
fn plan_is_identity(input: &JobInput) -> bool {
    blaze_units::query::env_var(&input.config.invocation.env, PLAN_IS_IDENTITY) == Some("1")
}

/// The first variable a kept front end read whose value `now`, a request's environment, has
/// another of: what its crates' `env!` and `option_env!` read
/// ([`blaze_lower::Lowered::env_moved`]), then what the daemon read into what it keeps with it
/// (`read`, its stores' modes, [`deps::Session::env_read`]). Nothing else of the environment
/// lets it go, so a no-op build from another shell or systemd unit lowers nothing again.
pub(crate) fn env_moved(
    lowered: &Lowered,
    read: &EnvReads,
    now: &[(String, String)],
) -> Option<EnvMoved> {
    lowered.env_moved(now).or_else(|| read.moved(now))
}

/// `blaze why`'s line for the kept front end of `kept` let go because `moved` did: a miss of
/// the step `front end` of its checkout, the read named with whether the variable was set,
/// unset or set to another value, and nothing made from a value. A hash of the value, even with
/// the name, would let whoever holds the build history try every short secret against it.
fn env_miss(kept: &KeptLowered, moved: &EnvMoved) -> blaze_kernel::Miss {
    let (was, now) = env_seen(moved);
    let read = base_db::process_env_reads(kept.lowered.db()).len()
        + kept.session.env_read().names().count();
    blaze_kernel::Miss {
        step: "front end",
        subject: ContentId::of("blaze v2 front end subject 1", kept.root.as_bytes()),
        item: kept.root.clone(), // clone: a miss, once per front end let go
        missed: blaze_kernel::Missed::Read {
            // alloc: as above
            read: moved.to_string(),
            names: None,
            was,
            now,
            changed: 1,
            reads: u32::try_from(read).unwrap_or(u32::MAX),
        },
        key: ContentId::default(),
        kept: None,
        parts: Vec::new(), // alloc: none
        beyond: None,
        via: None,
    }
}

/// What `blaze why` says a moved variable was and is: [`blaze_kernel::Seen::Gone`] when unset,
/// else a marker for "set" or, when it was set too, one for "set to another value": the same
/// for every variable and value, so it says which of the three changes it was and nothing of
/// the value.
fn env_seen(moved: &EnvMoved) -> (blaze_kernel::Seen, blaze_kernel::Seen) {
    const MARK: &str = "blaze v2 front end variable 2";
    let mark = |what: &[u8]| blaze_kernel::Seen::Is(ContentId::of(MARK, what));
    let was = match moved.was {
        Some(_) => mark(b"set"),
        None => blaze_kernel::Seen::Gone,
    };
    let now = match (&moved.was, &moved.now) {
        (_, None) => blaze_kernel::Seen::Gone,
        (Some(_), Some(_)) => mark(b"set to another value"),
        (None, Some(_)) => mark(b"set"),
    };
    (was, now)
}

/// The first part of two plans' units that differs, by name. Every field is named, so one
/// added to [`Units`] is named here too.
fn units_differ(kept: &Units, now: &Units) -> &'static str {
    let Units {
        graph,
        plans,
        packages,
        lock,
        warnings,
        notes,
        target_dir,
        build_dir,
        finished,
        link_dirs,
        sandboxed_tests,
        native,
    } = kept;
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
    } else if notes != &now.notes {
        "the load's notes"
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
    } else if native != &now.native {
        "the pinned native inputs"
    } else {
        "nothing compared"
    }
}

/// Lets go of the kept front end `kept`, which the build does not lower into because of
/// `why`, inside a span naming its checkout, why and the variable that moved when one did
/// (`variable`, empty when none: a profile query finds every drop a variable caused by it), as
/// dropping a whole database takes time, and says so in the daemon's log.
fn let_go(kept: KeptLowered, why: &str, variable: Option<&str>) {
    let checkout = kept.root.as_str();
    let variable = variable.unwrap_or("");
    let _span =
        tracing::info_span!(target: "blaze", "kept front end let go", checkout, why, variable)
            .entered();
    crate::serve::said::logged!(WARN,
        "blaze serve: the kept front end of {checkout} is let go, and its build lowers anew: {why}"
    );
    drop(kept);
}

/// Hands each of the kept `seeds` to its file as its next parse
/// ([`base_db::EditionedFileId::seed_parse`]), inside the refresh's write once the edit's
/// commit's texts are set and before its maintainers parse them. A seed refused (the text set
/// is not the one its tree was made of, or the file was parsed first) is an error naming the
/// file, never a whole parse in its place.
fn seed(db: &base_db::Db, seeds: &[blaze_intent::Seed]) -> Result<(), blaze_lower::LowerError> {
    for it in seeds {
        let extensions = it.parse.extensions();
        if !base_db::EditionedFileId::new_with_extensions(db, it.file, it.edition, extensions)
            .seed_parse(db, it.content, &it.parse)
        {
            // alloc: an error path.
            let at = db
                .path_for_file(it.file)
                .map_or_else(|| format!("{:?}", it.file), |path| path.to_string());
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

/// `Ok` once every proc macro `krate` can expand is built in the view `db` is of; otherwise an
/// error naming the answer (`work`: "reach", "edit", "ahead"), the target, the crate and the
/// proc macro. The backstop of the job's order, which builds and applies a view's proc macros
/// and build scripts' runs before any answer that reads its def maps unbuilt is posted
/// (`Jobs::next`, `reads_front_unbuilt`): an error here is a unit that order left out, never a
/// read of a half-expanded crate. The one check every such answer makes.
pub(crate) fn answer_macros_built(
    db: &Db,
    krate: base_db::Crate,
    target: &str,
    work: &str,
) -> Result<(), Failed> {
    blaze_items::macros_built(db, krate).map_err(|pending| {
        Failed::Error(ApiError::new(
            // alloc: error path
            blaze_report::say!(
                blaze_report::say::Words(&format!("the {work}'s answer in ")),
                blaze_report::say::Code(target),
                " can't be told before its proc macros are built, and it was posted with one not built (the job builds the proc macros and build scripts each view applies first): ",
                blaze_report::say::Words(&pending.describe(db).to_string())
            )
            .flat(),
            Some(work.to_owned()),
        ))
    })
}

impl KeptDb for KeptLowered {
    fn db(&self) -> Option<&Db> {
        Some(self.lowered.db())
    }
}

/// A job's front end as the loop keeps it ([`Writer::keep`]), with what it was lowered from:
/// the next build of the head lowers into it only when it lowers the same.
///
/// Counted while it is kept (`hir::evict::front_end_kept`), so the process tables every
/// front end's ids index are released only once none is, and
/// its drop retires its database's shared solver caches (`hir::evict::front_end_dropped`),
/// which no drop of a database did before. The next build of its head takes its parts
/// ([`KeptLowered::into_parts`]), which ends the count the same way.
pub struct KeptLowered {
    /// There until the next build of its head takes them.
    parts: Option<KeptParts>,
}

impl KeptLowered {
    /// `parts` kept: one front end more that the process tables wait for.
    fn new(parts: KeptParts) -> KeptLowered {
        hir::evict::front_end_kept();
        KeptLowered { parts: Some(parts) }
    }

    /// Its parts, for the next build of its head to lower into: it is no longer kept
    /// (`hir::evict::front_end_taken`). Its database's shared solver caches stay, for the build
    /// that takes it and the next at its revision; that build's own tasks hold a release of the
    /// process tables off until it keeps the database again or drops it.
    fn into_parts(mut self) -> KeptParts {
        let parts = self.parts.take().expect("a kept front end has its parts until they are taken, once");
        hir::evict::front_end_taken();
        parts
    }
}

impl Drop for KeptLowered {
    fn drop(&mut self) {
        if let Some(parts) = &self.parts {
            hir::evict::front_end_dropped(parts.lowered.db());
        }
    }
}

impl std::ops::Deref for KeptLowered {
    type Target = KeptParts;

    fn deref(&self) -> &KeptParts {
        self.parts.as_ref().expect("a kept front end has its parts until they are taken, once")
    }
}

impl std::ops::DerefMut for KeptLowered {
    fn deref_mut(&mut self) -> &mut KeptParts {
        self.parts.as_mut().expect("a kept front end has its parts until they are taken, once")
    }
}

/// How much of what a kept front end's build left for the store was written
/// ([`Steps::store_kept`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stored {
    /// Nothing yet.
    No,
    /// The build's end's store: the sealed crates' images, outcomes, constants, goals and
    /// bodies' steps (`hir_def::deps::Which::Sealed`). The next build may lower into it.
    Sealed,
    /// And the members' and the crates' above them (`hir_def::deps::Which::All`): what is
    /// written before the front end is let go for idleness, and at a stop.
    All,
}

/// What a [`KeptLowered`] keeps.
pub struct KeptParts {
    /// What codegen's walks over its database keep for the next build of it
    /// ([`hir_ty_codegen::v2::Walks`]): held here and nowhere else, so they leave with the kept
    /// front end. Declared first: they name the database's ids, and drop before it.
    walks: Arc<hir_ty_codegen::v2::Walks>,
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
    /// How much of what its build left for the store was written ([`Steps::store_kept`]): the
    /// next build lowers into it only once the build's end stored it ([`Stored::Sealed`]).
    stored: Stored,
    /// What its store's flushes did, for the `-v` note of the next build lowering into it.
    flushed: Option<deps::Flushes>,
    /// The trees its job's last edit reparsed in place, for the next build to seed its parses
    /// with when it moves to the edit's commit.
    seeds: Vec<blaze_intent::Seed>,
    /// What its job's builds used of the code generator's memo, kept there while it is kept;
    /// taken by the next job of the head.
    memo: Option<memo::Held>,
    /// What its build's end lets go of it ([`BuildEnd`]), until the task that stores it runs
    /// the release or leaves it for a request that waits ([`Steps::release_kept`]).
    end: Option<BuildEnd>,
}

/// What a task reads the front end through ([`Steps::Reader`]): a shared reference to the job's
/// lowered front end, as it stood when the loop posted the task. Its database is read by
/// reference, never cloned.
pub struct UnitReader {
    /// What codegen's walks over the database keep ([`KeptParts::walks`]): a unit's codegen
    /// patches its program's kept walk in it and keeps its own there.
    walks: Arc<hir_ty_codegen::v2::Walks>,
    front: Arc<Lowered>,
    /// The dependencies' check outcomes its database was served ([`deps::serve`]): written
    /// with the front end, only while no reader holds it.
    checks: Arc<blaze_diagnostics::cache::Served>,
    /// Each crate's body entries kept between serves ([`deps::Session::bodies`]).
    bodies: Arc<crate::serve::front_carry::bodies::Sessions>,
}

impl UnitReader {
    pub(crate) fn db(&self) -> &Db {
        self.front.db()
    }

    /// What codegen's walks over the database keep for its next build.
    pub(crate) fn walks(&self) -> &hir_ty_codegen::v2::Walks {
        &self.walks
    }

    /// What unit `unit`'s identity is made of ([`Lowered::identity_parts`]), for `blaze why`.
    pub(crate) fn identity_parts(
        &self,
        unit: usize,
        looked: blaze_ids::ContentId,
    ) -> Result<Vec<blaze_lower::IdentityPart>, String> {
        self.front.identity_parts(unit, looked)
    }

    /// What unit `unit`'s narrowed identity holds of each crate of its closure
    /// ([`Lowered::narrow_parts`]), for `blaze why`.
    pub(crate) fn narrow_parts(&self, unit: usize) -> Vec<(String, blaze_ids::ContentId)> {
        self.front.narrow_parts(unit)
    }

    pub(crate) fn checks(&self) -> &blaze_diagnostics::cache::Served {
        &self.checks
    }

    /// Each crate's body entries kept between serves, for a unit's serve and keep of them.
    pub(crate) fn bodies(&self) -> &crate::serve::front_carry::bodies::Sessions {
        &self.bodies
    }

    /// The job's files by path.
    pub(crate) fn paths(&self) -> &Arc<HashMap<FileId, String>> {
        self.front.paths()
    }

    /// How cargo passes the job's crates' roots, which the program's names of its files are
    /// made from ([`blaze_lower::Spellings::name`]).
    pub(crate) fn spellings(&self) -> &Arc<blaze_lower::Spellings> {
        self.front.spellings()
    }

    /// The file of each of `names` (the files diagnostics name, from `root` as
    /// [`blaze_diagnostics::Files::relative`] names them, with the identity of a text that is
    /// not the file at its path as [`blaze_diagnostics::Files::of_text`] adds it) that the
    /// database has the text of, by name: a file of [`UnitReader::paths`] whose name is that
    /// whole name, so a crate root with an appended module and the file on disk it is an alias
    /// of are never taken for one another, the first by ID of two of one name, else the one
    /// `include!` parsed of a file outside every source root at that path
    /// ([`base_db::Files::included_source_file`]), which no path of the job names.
    pub(crate) fn files_named(&self, root: &str, names: &[&str]) -> Vec<(String, FileId)> {
        let db = self.db();
        let mut found: Vec<(String, FileId)> = (self.paths().iter())
            .filter(|&(&file, _)| db.has_file_text(file))
            .map(|(&file, path)| (blaze_diagnostics::Files::relative(path, root), file))
            .filter(|(name, _)| names.iter().any(|it| blaze_diagnostics::emit::shown(it) == *name))
            // alloc: the few names of a unit's diagnostics
            .map(|(name, file)| {
                (blaze_diagnostics::Files::of_text(db, file, name.to_owned()), file)
            })
            .filter(|(name, _)| names.contains(&name.as_str()))
            .collect(); // alloc: as above
        // The first file of a name by ID, whatever order the map is in.
        found.sort_by(|a, b| (&a.0, a.1.index()).cmp(&(&b.0, b.1.index())));
        found.dedup_by(|a, b| a.0 == b.0);
        for &name in names {
            // A text that is not the file at its path is no file `include!` read either.
            if found.iter().any(|(it, _)| it == name) || blaze_diagnostics::emit::names_text(name) {
                continue;
            }
            let whole = match name.starts_with('/') {
                true => name.to_owned(),                                   // alloc: as above
                false => format!("{}/{name}", root.trim_end_matches('/')), // alloc: as above
            };
            let Ok(whole) = base_db::AbsPathBuf::try_from(whole.as_str()) else { continue };
            if let Some(file) = db.files().included_source_file(&whole) {
                found.push((name.to_owned(), file)); // alloc: as above
            }
        }
        found
    }
}

/// The job's front end as only the loop writes it: the one reference not lent to a reader,
/// the writes asked for and not yet applied, and as each build script's run is applied, the
/// identities of the units it was the last run in the closure of.
pub struct UnitWriter {
    /// What codegen's walks over the database keep ([`KeptParts::walks`]), declared first as
    /// there.
    walks: Arc<hir_ty_codegen::v2::Walks>,
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
    /// Device units' MSL asked for, checked, in the order asked.
    msls: Vec<(usize, Vec<u8>)>,
    /// What this job's serves served of crates whose key moved ([`deps::serve`]), for the front
    /// end's images to be stored with it: none when no key moved.
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
    /// What the build's end lets go of its front end, told as its units compile ([`BuildEnd`]).
    end: BuildEnd,
}

/// The most crates a build may make and still be an edit's ([`BuildEnd::release`]): the crate
/// edited and the few a changed interface reaches at once.
const EDIT_CRATES: usize = 4;

/// What a build's end lets go of its kept front end, run after the build's answer by the task
/// that stores it ([`Steps::release_kept`]), with the front end the task's alone. Nothing lets
/// go of the front end mid-build: until 2026-11-13 a release ran as a write of the job's
/// writer, which stopped every reader for it, whenever no unit was open or memory was short
/// (`unit::release`, deleted with the memory gate). A value let go keeps its memo, so a read
/// of it by the next build makes it again and backdates it: nothing read can differ.
///
/// - A check keeps its inference, which the next check of the head reads again; its MIR goes.
/// - An edit's build (it found a unit kept and made at most [`EDIT_CRATES`] crates) keeps its
///   inference and MIR, which the next edit's check reads again (the lints of bodies), and
///   lets go of the trait solver's caches, which are of this revision alone.
/// - Any other build lets go of every MIR and of the inference of the dependencies it
///   finished (no workspace member's, `CrateOrigin::Local`: those an edit makes again).
///
/// Then, at every end, the storage's upkeep: [`blaze_query::db::Storage::end_epoch`] lets go
/// of what neither this build nor the one before read, the reverse edges are packed and the
/// changed set settled.
struct BuildEnd {
    /// Each unit's crate; none for a build script's run.
    of: Vec<Option<base_db::Crate>>,
    /// Each unit found kept as it began ([`Writer::found_kept`]).
    kept: Vec<bool>,
    /// Each unit whose compile ended ([`Writer::compiled`]).
    compiled: Vec<bool>,
    /// Whether the build compiles code: none for a check.
    compiles: bool,
}

impl BuildEnd {
    fn new(crates: &Crates, units: usize, compiles: bool) -> BuildEnd {
        BuildEnd {
            of: (0..units).map(|unit| crates.krate(unit)).collect(), // alloc: a crate a unit, once per build
            kept: vec![false; units],     // alloc: a flag a unit, once per build
            compiled: vec![false; units], // alloc: a flag a unit, once per build
            compiles,
        }
    }

    /// How many crates the build made: those of a compiled unit not found kept.
    fn made(&self) -> usize {
        let mut made = blaze_hash::HashMap::new_in(std::alloc::Global);
        for unit in 0..self.of.len() {
            if let Some(krate) = self.of[unit]
                && self.compiled[unit]
                && !self.kept[unit]
            {
                made.insert(krate, ());
            }
        }
        made.len()
    }

    /// The crates every unit of which compiled, in the order of their first unit.
    fn finished(&self) -> Vec<base_db::Crate> {
        // Each crate's place in `crates` and whether every unit of it compiled so far.
        let mut seen = blaze_hash::HashMap::new_in(std::alloc::Global);
        let mut crates: Vec<(base_db::Crate, bool)> = Vec::new(); // alloc: a crate a build, once per build
        for unit in 0..self.of.len() {
            let Some(krate) = self.of[unit] else { continue };
            let at = *seen.entry(krate).or_insert_with(|| {
                crates.push((krate, true));
                crates.len() - 1
            });
            crates[at].1 &= self.compiled[unit];
        }
        crates.into_iter().filter(|it| it.1).map(|it| it.0).collect() // alloc: as above
    }

    /// Lets go of what the build's end lets go (the type's doc), under a span naming it.
    fn release(self, lowered: &mut Lowered) {
        use blaze_query::db::Database as _;

        let db = lowered.served_db();
        let found_kept = self.kept.iter().any(|&it| it);
        let edit = self.compiles && found_kept && self.made() <= EDIT_CRATES;
        let crates: Vec<base_db::Crate> = match (self.compiles, edit) {
            (false, _) | (true, true) => Vec::new(), // alloc: none, empty
            (true, false) => {
                let db: &base_db::Db = &*db;
                let mut finished = self.finished();
                finished.retain(|krate| !krate.data(db).origin.is_local());
                finished
            }
        };
        let mir = !edit;
        let span = tracing::info_span!(
            target: "blaze",
            "front end release at the build's end",
            edit,
            crates = crates.len(),
            mir,
            inference = tracing::field::Empty,
            mir_dropped = tracing::field::Empty,
            solver_caches = tracing::field::Empty,
            unread = tracing::field::Empty,
            query_storage = tracing::field::Empty,
        );
        let _release = span.enter();
        let mut dropped = hir::evict::drop_crates_after_store(db, &crates, mir);
        if edit {
            dropped.solver_caches += hir::evict::drop_every_solver_cache(db);
        }
        let unread = db.storage_mut().end_epoch();
        db.storage_mut().pack_reverse();
        // What its pulls left for the next changed-set walk is settled: each row the build
        // made again said it changed, and the next edit's walk would start from all of them.
        db.storage_mut().settle_changed_set();
        if std::env::var_os("BLAZE_DURABILITY_CENSUS").is_some() {
            crate::serve::said::logged!(INFO,
                "blaze serve: durability census: {}",
                db.storage().durability_census()
            );
        }
        span.record("inference", dropped.inference);
        span.record("mir_dropped", dropped.mir);
        span.record("solver_caches", dropped.solver_caches);
        span.record("unread", unread.values);
        span.record("query_storage", db.storage().held_bytes());
    }
}

/// `broke`, a failure or a panic of a step of an apply for each of the units `ready`, kept
/// once for each. `itself`: the step was part of making those units (their harness, their
/// dependencies served), so each is refused too, not only what depends on it.
fn broke_each(into: &mut Vec<Broke>, broke: &Broke, ready: &[u32], itself: bool) {
    for &unit in ready {
        // clone: a failure, once per unit it fails
        into.push(Broke { unit: unit as usize, what: broke.what, how: broke.how.clone(), itself });
    }
}

impl Writer<Real> for UnitWriter {
    fn reader(&self) -> Option<UnitReader> {
        (!self.waiting())
            .then(|| UnitReader {
                walks: Arc::clone(&self.walks),
                front: Arc::clone(&self.front),
                checks: self.session.checks(),
                bodies: self.session.bodies(),
            })
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

    fn gpu_msl(&mut self, unit: usize, msl: Vec<u8>) -> Result<(), ApiError> {
        self.front.check_gpu_msl(unit).map_err(lower_error)?;
        self.msls.push((unit, msl));
        Ok(())
    }

    fn waiting(&self) -> bool {
        !self.runs.is_empty() || !self.macros.is_empty() || !self.msls.is_empty()
    }

    fn idle(&self) -> bool {
        Arc::strong_count(&self.front) == 1
    }

    fn write_wait(&self) -> WriteWait {
        let count = |it: usize| u32::try_from(it).unwrap_or(u32::MAX);
        WriteWait {
            runs: count(self.runs.len()),
            macros: count(self.macros.len()),
            msls: count(self.msls.len()),
            readers: count(Arc::strong_count(&self.front).saturating_sub(1)),
        }
    }

    fn found_kept(&mut self, unit: usize) {
        if let Some(kept) = self.end.kept.get_mut(unit) {
            *kept = true;
        }
    }

    fn compiled(&mut self, unit: usize) {
        if let Some(compiled) = self.end.compiled.get_mut(unit) {
            *compiled = true;
        }
    }

    /// The writes asked for since the last apply, as three changes, each one maintenance pass
    /// of the database ([`base_db::Trigger`] names each on its `maintain` span), not one per
    /// input set: every proc macro's load and build script's run in one batch; then, reading
    /// the database as those left it, the harness and glue of every unit they reached or made
    /// ready in one ([`harness::write`]), before those units are identified; then the images of
    /// the units made ready or reached by a loaded proc macro ([`deps::serve`]), in a front end
    /// kept or fresh.
    fn apply(&mut self) -> Result<bool, ApiError> {
        if !self.waiting() {
            return Ok(true);
        }
        let Some(lowered) = Arc::get_mut(&mut self.front) else { return Ok(false) };
        self.ready.clear();
        // alloc: once an apply, the units it writes, named on its `maintain` span
        let touched: Vec<u32> = self
            .macros
            .iter()
            .map(|(unit, _)| *unit as u32)
            .chain(self.runs.iter().map(|(run, _)| *run as u32))
            .chain(self.msls.iter().map(|(unit, _)| *unit as u32))
            .collect();
        let trigger_of =
            touched.iter().map(|&unit| lowered.what(unit as usize)).collect::<Vec<_>>().join(", "); // alloc: as `touched`
        let trigger =
            Trigger::of("proc macros loaded, build scripts' runs and device builds' MSL applied", &trigger_of);
        // alloc: once an apply, the proc macros it loaded
        let mut loaded: Vec<u32> = Vec::with_capacity(self.macros.len());
        let (waits, ready, broke, macros, runs, msls) = (
            &mut self.waits,
            &mut self.ready,
            &mut self.broke,
            &mut self.macros,
            &mut self.runs,
            &mut self.msls,
        );
        let first = touched[0] as usize;
        // Each write's failure, an error or a panic, is its unit's ([`Writer::broke`]): the
        // writes after it are still applied, and only the units that depend on it are refused.
        let applied = applying(first, "the proc macros' loads and build scripts' runs", || {
            lowered.batch(trigger, |lowered| {
                for (unit, macros) in macros.drain(..) {
                    match applying_fallible(unit, "a proc macro's load", || {
                        lowered.macros(unit, macros).map_err(lower_error)
                    }) {
                        Ok(()) => loaded.push(unit as u32),
                        Err(it) => broke.push(it),
                    }
                }
                for (run, ran) in runs.drain(..) {
                    let from = ready.len();
                    let applied = applying_fallible(run, "a build script's run", || {
                        lowered.ran(run, &ran).map_err(lower_error)?;
                        waits.applied(run as u32, ready);
                        Ok(())
                    });
                    if let Err(it) = applied {
                        ready.truncate(from);
                        broke.push(it);
                    }
                }
                // A device unit's MSL is, to its dependents, what a run is: what their crates
                // read, once applied ([`graph::flag::GPU_MSL`]).
                for (unit, msl) in msls.drain(..) {
                    let from = ready.len();
                    let applied = applying_fallible(unit, "a device build's MSL", || {
                        lowered.gpu_msl(unit, &msl).map_err(lower_error)?;
                        waits.applied(unit as u32, ready);
                        Ok(())
                    });
                    if let Err(it) = applied {
                        ready.truncate(from);
                        broke.push(it);
                    }
                }
            })
        });
        if let Err(it) = applied {
            // The pass at the batch's end panicked: every write of it fails.
            broke_each(&mut self.broke, &it, &touched, false);
            self.ready.clear();
            return Ok(true);
        }
        // The harnesses: of the units these runs made ready, and of every ready unit a proc
        // macro just loaded reaches ([`harness::reached`]), in one write.
        // Also the units whose dependencies are served again: a proc macro built into other
        // code moves the key of every crate it reaches.
        let (job, waits, ready) = (&self.job, &self.waits, &self.ready);
        let mut written = harness::reached(job, waits, &loaded, ready);
        if let Some(&first) = written.first() {
            match applying(first as usize, "the harness", || {
                harness::write(lowered, job, written.iter().copied())
            }) {
                Ok(Ok(())) => {}
                // A root that can't be made is its unit's alone ([`harness::write`]): that unit
                // is refused, and what depends on it, naming it; every other unit's harness was
                // written, and it goes on.
                Ok(Err(refused)) => {
                    for (unit, error) in refused {
                        self.broke.push(Broke {
                            itself: true,
                            ..Broke::failed(unit, "the harness", error)
                        });
                        written.retain(|&it| it as usize != unit);
                        self.ready.retain(|&it| it as usize != unit);
                    }
                }
                // It panicked, making no one knows which root: every unit of the write is refused.
                Err(it) => {
                    broke_each(&mut self.broke, &it, &written, true);
                    self.ready.clear();
                    return Ok(true);
                }
            }
        }
        for &unit in &self.ready {
            identify(lowered, job, unit)?;
        }
        // The units these runs made ready and those a loaded proc macro reaches are served, in a
        // kept front end as in a fresh one: a run applied again with other cfgs, env or
        // `OUT_DIR` files, or a macro built into other code, moved the keys of the crates it
        // reaches, and a crate read from an image under its old key would resolve as it did
        // before (rule 21). A serve that fails or panics has left the front end partly served,
        // which no unit's write owns: it is the writer's own error, and ends the job with or
        // without keep-going, naming the units it served; no unit is built over it.
        if let Err(error) =
            serve_written(lowered, &self.job, &written, &mut self.session, &mut self.deps)
        {
            self.broken = true;
            return Err(error);
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
            let what =
                "the front end's tables are measured with no task reading it, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| lowered.tables())
    }

    fn finished(
        &mut self,
        ok: bool,
        built: &[u32],
    ) -> Result<Vec<blaze_api::Diagnostic>, ApiError> {
        self.deps.as_ref().map_or(Ok(()), deps::differences)?;
        deps::item_errors(&self.session)?;
        let mut said: Vec<blaze_api::Diagnostic> =
            codegen::unapplied(&self.job, ok, built)?.into_iter().map(warning).collect();
        said.extend(configs::report(&self.job, self.reader(), ok, built)?);
        Ok(said)
    }

    fn served(&self) -> Result<Vec<blaze_api::ServedCrate>, ApiError> {
        deps::served(self.front.db(), &self.job, &self.session)
    }

    fn run_log(&self) -> blaze_query::db::Taken {
        hir_def::span_names::take_run_log(self.front.db())
    }

    fn goals_noted(&self) -> Option<blaze_api::Diagnostic> {
        deps::noted(self.flushed.as_ref(), self.session.item_steps().as_ref())
    }

    /// The sources of the view every key of this build held by code text alone: each Rust
    /// file of a workspace package all of whose units were found under their narrow record
    /// key ([`compile::found_narrow`]; `blaze_lower::Lowered::still_sources` says the rest),
    /// less every file a build script's kept run read ([`script::ReadBytes`], by its last
    /// name), by its path from the view's root. None, with the reason, when a build script's
    /// run was not found kept by `fresh` (its reads are not noted), when one hashed a whole
    /// directory, or when the front end vouches for none.
    fn still(&self) -> Result<Vec<(String, ::span::Edition)>, String> {
        let job = &self.job;
        let reads = job.script_reads.lock();
        for (unit, it) in job.units().graph.units.iter().enumerate() {
            if it.mode == Mode::RunCustomBuild && !reads.noted(unit) {
                // alloc: a reason, once a build
                return Err(format!(
                    "what the build script of {} read is not noted: its run was not found kept as the build began",
                    it.pkg_id
                ));
            }
        }
        if reads.trees {
            // alloc: a reason, once a build
            return Err("a build script reads a whole directory by its bytes".to_owned());
        }
        let sources = self
            .front
            .still_sources(|unit| compile::found_narrow(job, unit))
            .map_err(str::to_owned)?; // alloc: a reason, once a build
        let root = job.input.config.invocation.root.trim_end_matches('/');
        Ok(sources
            .into_iter()
            .filter_map(|it| {
                let path = it.path.strip_prefix(root)?.strip_prefix('/')?;
                let name = path.rsplit('/').next().unwrap_or(path);
                (!reads.reads(name)).then(|| (path.to_owned(), it.edition)) // alloc: the edge
            })
            .collect()) // alloc: once a build that walked and made nothing
    }

    /// The variables of the request's environment this build's steps asked for by name:
    /// what each build script's kept run was checked against by name ([`script::ReadBytes`],
    /// noted as [`script::fresh`] found it kept), what any unit record of this process was
    /// looked up or filed by ([`reads::named_env`]: its crates' `env!` and `option_env!`),
    /// what the front end's crates read that way ([`base_db::process_env_reads`]) and what the
    /// daemon read into what it keeps with the front end ([`deps::Session::env_read`]). More
    /// names than this build's own is sound (a name more is a variable more compared); a
    /// name less is not, so: none, with the reason, when a build script's run was neither
    /// found kept by `fresh` nor kept by this build (`script::ReadBytes::note_ran`): its
    /// reads are not noted.
    fn env_named(&self) -> Result<Vec<String>, String> {
        let job = &self.job;
        let noted = job.script_reads.lock();
        for (unit, it) in job.units().graph.units.iter().enumerate() {
            if it.mode == Mode::RunCustomBuild && !noted.noted(unit) {
                // alloc: a reason, once a build
                return Err(format!(
                    "the variables the build script of {} read are not noted: its run was neither found kept as the build began nor kept by this build",
                    it.pkg_id
                ));
            }
        }
        let mut names = noted.env.clone(); // clone: once a build that walked and made nothing
        drop(noted);
        names.extend(reads::named_env());
        names.extend(base_db::process_env_reads(self.front.db()).into_iter().map(|(_, name)| name));
        // alloc: a name each, once a build that walked and made nothing
        names.extend(self.session.env_read().names().map(|(name, _)| name.to_owned()));
        Ok(names)
    }

    /// The paths under this build's directories of outputs that its steps read: what each
    /// build script's kept run was checked against there ([`script::ReadBytes::paths`],
    /// [`script::ReadBytes::listed`]), and every file and listing any unit record of this
    /// process was filed by ([`reads::read_paths`]: `include!`d files outside the source
    /// roots, what proc-macro expansions read). None, with the reason, when a build script's
    /// run was not found kept by `fresh`, or a record's expansion reads do not decode.
    fn read_outside(&self) -> Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String> {
        let job = &self.job;
        let noted = job.script_reads.lock();
        for (unit, it) in job.units().graph.units.iter().enumerate() {
            if it.mode == Mode::RunCustomBuild && !noted.noted(unit) {
                // alloc: a reason, once a build
                return Err(format!(
                    "the paths the build script of {} read are not noted: its run was neither found kept as the build began nor kept by this build",
                    it.pkg_id
                ));
            }
        }
        // clone: once a build that walked and made nothing
        let (mut files, mut listed) = (noted.paths.clone(), noted.listed.clone());
        drop(noted);
        let (record_files, record_listed) = reads::read_paths()?;
        files.extend(record_files);
        listed.extend(record_listed);
        Ok((files, listed))
    }

    /// Every read of the machine's files a build script's record rests on, with what was
    /// there when the record held ([`script::ReadBytes::seen`], where the completeness of
    /// the list is argued). None, naming the unit, when a build script of the build has no
    /// trace to say it: one run outside the sandbox, one that is checked (run again whatever
    /// is kept), one whose run was neither found kept nor kept by this build, or one whose
    /// check walked a directory by a question no read is of.
    fn read_seen(&self) -> Result<Vec<crate::serve::work::Seen>, String> {
        let job = &self.job;
        let noted = job.script_reads.lock();
        for (unit, it) in job.units().graph.units.iter().enumerate() {
            if it.mode == Mode::RunCustomBuild && !noted.noted(unit) {
                // alloc: a reason, once a build
                return Err(format!(
                    "the build script of {} has no trace of what it read: its run was neither found kept as the build began nor kept by this build",
                    it.pkg_id
                ));
            }
        }
        if let Some(path) = &noted.unseen {
            // alloc: a reason, once a build
            return Err(format!(
                "a build script's kept run was checked against {} by its names or one entry alone, which no read of a certificate is of",
                path.display()
            ));
        }
        Ok(noted.seen.clone()) // clone: once a build, a read a file its build scripts read
    }

    fn view_crates(&self) -> Result<Vec<crate::serve::front_carry::CrateRoot>, String> {
        let job = &self.job;
        let units = &job.input.units;
        let db = self.front.db();
        // alloc: once a build, a crate of the view each
        let mut out = Vec::new();
        for (at, unit) in units.graph.units.iter().enumerate() {
            if unit.is_std || unit.mode == Mode::RunCustomBuild {
                continue;
            }
            let Some(krate) = self.front.krate(at) else { continue };
            let Some(src) = unit.target.src_path.to_str() else {
                // alloc: an error
                return Err(blaze_report::say!(
                    blaze_report::say::Words(&format!("unit {at}'s root ")),
                    blaze_report::say::Code(&unit.target.src_path.display().to_string()),
                    " is not UTF-8"
                )
                .flat());
            };
            // A crate whose root is outside the view is another root's (a dependency's).
            let Some(root) = job.mounted.in_view(src) else { continue };
            let package = units
                .packages
                .binary_search_by(|it| it.id.as_str().cmp(&unit.pkg_id))
                .map(|place| units.packages[place].name.as_str())
                .map_err(|_| {
                    blaze_report::say!(blaze_report::say::Words(&format!("unit {at}'s package ")), blaze_report::say::Code(&unit.pkg_id.to_string()), " is not in the plan").flat()
                })?; // alloc: an error
            let edition = blaze_lower::unit_edition(unit).map_err(|error| error.to_string())?;
            // alloc: once a crate a build
            let what = format!("{package} {} {}{}", unit.target.name, unit.target.kind.join(","), match unit.checked_as_test || unit.mode == Mode::Test {
                true => " test",
                false => "",
            });
            // The cfg as the build ended it: the lowering's, with every build script's run
            // applied (`blaze_lower`'s apply of a run's `cargo:rustc-cfg`s sets it).
            // clone: once a crate a build
            let cfg = krate.cfg_options(db).clone();
            let extensions = krate.extensions(db);
            out.push(crate::serve::front_carry::CrateRoot::new(&what, root.into_owned(), edition, cfg, extensions));
        }
        Ok(out)
    }

    fn keep(mut self) -> Result<KeptLowered, ApiError> {
        // The writes still asked for are applied. With none, only that no reader is alive is
        // asked: the build's end's release is the task's that stores the front end.
        let applied = match self.runs.is_empty() && self.macros.is_empty() {
            true => Arc::get_mut(&mut self.front).is_some(),
            false => self.apply()?,
        };
        if !applied {
            let what = "the front end is kept for the head's next build with no task reading it, and one still does";
            return Err(self.refused(what));
        }
        if self.broken || !self.broke.is_empty() {
            let what = "the front end is not kept for the head's next build: a write to it panicked, and it may be half written";
            return Err(self.refused(what));
        }
        // The build is over: what its end lets go ([`BuildEnd`]) goes, not here, on the loop
        // before the build's answer, but with the front end kept, by the task that stores it,
        // after the answer ([`Steps::release_kept`]).
        let UnitWriter { walks, session, front, job, deps, retractions, end, .. } = self;
        let Some(lowered) = Arc::into_inner(front.into_inner()) else {
            let what = "the front end is kept for the head's next build, and another holder of it is still alive";
            return Err(ApiError::new(what, None));
        };
        let later = std::mem::take(&mut *job.later.lock());
        let seeds = std::mem::take(&mut *job.seeds.lock());
        // alloc: once per front end kept, every entry its units' builds used
        let used: Vec<ContentId> =
            job.memo_used.iter().filter_map(|it| it.get()).flatten().copied().collect();
        let memo = Some(job.shared.memos.hold(used));
        Ok(KeptLowered::new(KeptParts {
            walks,
            session,
            lowered: Held::new(lowered, "a kept front end"),
            units: job.input.units.clone(),
            config: job.input.config.clone(),
            deps,
            retractions,
            later,
            root: job.mounted.root().to_owned(), // alloc: once per front end kept
            stored: Stored::No,
            flushed: None,
            seeds,
            memo,
            end: Some(end),
        }))
    }
}

impl UnitWriter {
    /// The error of a front end [`Writer::keep`] refuses, which it lets go of here: the panic
    /// of that drop, if it panics, is said with the refusal and fails the job with it.
    fn refused(self, what: &str) -> ApiError {
        let UnitWriter { walks, session, front, .. } = self;
        // The walks first: they name the database's ids.
        drop((walks, session));
        match front.let_go() {
            Ok(()) => ApiError::new(what, None),
            // alloc: an error, once
            Err(panic) => ApiError::new(format!("{what}; letting go of it then {panic}"), None),
        }
    }
}

/// A front end's database as the loop holds it ([`UnitWriter`], [`KeptLowered`]). The last
/// drop of one drops a whole query storage, which may panic (a table it can't walk), and a
/// panic on the loop would end every request the daemon serves (rule 18). Its drop runs inside
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
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(it)))
        .map_err(|payload| super::plane::panicked(&*payload))
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
            crate::serve::said::logged!(ERROR,
                "blaze serve: letting go of {} {panic}; the daemon goes on, the memory it held may be lost",
                self.what
            );
        }
    }
}

/// Unit `unit`'s identity set from the front end, once every run of its closure is applied:
/// from then on nothing changes what its crate is made from.
///
/// The library crates of its closure (a registry's, a git's) are given their own narrowed
/// identities first ([`Lowered::library_narrows`]), each read from the store under its key, or
/// worked out by parsing that crate where the store has none and left in
/// [`Job::library_narrows`] to be kept. A read of the store that fails, or bytes there that are
/// no narrowed identity, is the error (rule 3).
fn identify(lowered: &mut Lowered, job: &Job, unit: u32) -> Result<(), ApiError> {
    // The narrowed identities first: a reader that finds the identity set finds them set.
    if let Some(slot) = job.narrowed.get(unit as usize) {
        // On the loop (`UnitWriter::apply`): each part a span of its own, with how many
        // library crates were read from the store and how many parsed in full for want of a
        // narrowed identity there (rule slow-is-p0-traced: a turn stuck 60 s in this span
        // named neither, 2026-11-13).
        let span = tracing::info_span!(
            target: "blaze",
            "unit narrowed identity",
            unit,
            read = tracing::field::Empty,
            parsed = tracing::field::Empty,
        )
        .entered();
        let front_end = deps::front_end()?;
        let mut read = 0_u64;
        // One direct read into the identity's own few bytes, which the front end keeps.
        let kept = |key: &ContentId| {
            read += 1;
            let _read =
                tracing::info_span!(target: "blaze", "unit narrowed identity: a store read").entered();
            match super::direct::owned(&job.input.reads, super::direct::Which::Home, &blaze_cas::Key(*key)) {
                // alloc: a narrowed identity's few bytes, once per library crate per front end
                Ok(found) => Ok(found),
                Err(failed) => Err(failed.to_string()), // alloc: error path
            }
        };
        let library = tracing::info_span!(
            target: "blaze",
            "unit narrowed identity: its library crates (store reads, parses where the store has none)"
        )
        .entered();
        lowered.library_narrows(unit as usize, &front_end, kept).map_err(lower_error)?;
        drop(library);
        let made = lowered.take_library_narrows();
        span.record("read", read);
        span.record("parsed", made.len() as u64);
        if !made.is_empty() {
            job.library_narrows.lock().extend(made.into_iter().map(|(key, bytes)| Put {
                key: blaze_cas::Key(key),
                kind: blaze_cas::Kind::Query,
                bytes,
            }));
        }
        let own = tracing::info_span!(
            target: "blaze",
            "unit narrowed identity: its closure's texts (each crate's files parsed once a revision)"
        )
        .entered();
        let _ = slot.set(lowered.narrowed(unit as usize));
        drop(own);
        drop(span);
    }
    if let Some(slot) = job.identities.get(unit as usize) {
        let _span = tracing::info_span!(target: "blaze", "unit identity", unit).entered();
        let _ = slot.set(lowered.identity(unit as usize));
    }
    Ok(())
}

/// A shipped tree blaze placed (`toolchain::library::owned`) as the [`Library`] mounted at
/// `named`: its texts file mapped ([`crate::toolchain::library::open_texts`]), made first when an
/// older blaze placed the tree without one, and each file's bytes lent from the mapping with no
/// copy and no file of the tree read. Its fingerprint is of the digest and the listing's hash;
/// `kept` holding it at `named` returns no source, as a library read file by file does.
fn library_from_texts(
    tree: &std::path::Path,
    digest: &str,
    named: String,
    kept: &[(String, String)],
) -> Result<Library, ApiError> {
    use crate::toolchain::library::{open_texts, write_texts};
    let _s = tracing::info_span!(target: "blaze", "std library: map its texts").entered();
    let in_tree = |what| ApiError::new(what, Some(tree.display().to_string()));
    let (mapping, listing) = match open_texts(tree, digest).map_err(in_tree)? {
        Some(opened) => opened,
        None => {
            tracing::info!(
                target: "blaze",
                tree = %tree.display(),
                "a shipped tree placed with no texts file (by an older blaze): its texts are written once"
            );
            write_texts(tree, digest).map_err(in_tree)?;
            open_texts(tree, digest).map_err(in_tree)?.ok_or_else(|| {
                in_tree(format!(
                    "the texts of the shipped tree at {} were written and are not there",
                    tree.display()
                ))
            })?
        }
    };
    let mut hasher = blaze_ids::Hasher::new("blaze v2 serve library fingerprint 2");
    hasher.part(digest.as_bytes()).part(&listing.hash);
    let fingerprint = format!("{:x}", hasher.finish());
    if kept.iter().any(|(it, mounted)| *it == fingerprint && *mounted == named) {
        return Ok(Library { at: named, fingerprint, source: None });
    }
    // alloc: the source's files, once a mount of the tree
    let mut files = Vec::with_capacity(listing.entries.len());
    for entry in listing.entries {
        let lent = usize::try_from(entry.at)
            .ok()
            .zip(usize::try_from(entry.at + entry.len).ok())
            .and_then(|(at, end)| blaze_vfs::lent(&mapping, at..end));
        let bytes = lent.ok_or_else(|| {
            in_tree(format!(
                "{}: its text at {}..{} is not inside the texts file's {} bytes",
                entry.path,
                entry.at,
                entry.at + entry.len,
                mapping.bytes().len()
            ))
        })?;
        // The listing's ID, made from these bytes when the texts file was written: the mount
        // hashes nothing (`Vfs::apply_known_files`).
        files.push(SourceFile { path: entry.path, id: entry.id, bytes, mode: entry.mode });
    }
    Ok(Library { at: named, fingerprint, source: Some(Source { files, under: String::new() }) })
}

/// Every file under the library's `dir`, by its path from there. A link is its target's bytes:
/// the library's digest (`library::digest_of`) was checked when it was found or fetched. Only
/// for a library blaze did not place (one `BLAZE_LIBRARY` names, or one beside the program): a
/// tree blaze placed is mounted from its texts ([`library_from_texts`]).
fn library_files(
    dir: &std::path::Path,
    prefix: &str,
    into: &mut Vec<SourceFile>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let full = entry.path();
        let name = entry.file_name();
        let name =
            name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", full.display()))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let meta =
            std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        if meta.is_dir() {
            library_files(&full, &path, into)?;
            continue;
        }
        let bytes = SourceFile::read(&full, meta.len()).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable { blaze_vfs::Mode::Executable } else { blaze_vfs::Mode::Regular };
        into.push(SourceFile::of(path, bytes, mode));
    }
    Ok(())
}

/// Every file under the library's `dir` as [`library_files`] reads it, but looked at only:
/// its path, size, time of its last change and whether it is executable, in name order. Only
/// for a library [`library_files`] reads.
fn library_fingerprint(
    dir: &std::path::Path,
    prefix: &str,
    into: &mut blaze_ids::Hasher,
) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("{}: a name that is not UTF-8", entry.path().display()))?;
        entries.push((name.to_owned(), entry.path()));
    }
    blaze_ids::names::sort_by_name(&mut entries, |it| it.0.as_bytes());
    for (name, full) in entries {
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let meta =
            std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
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
/// ([`hir_def::nameres::stopped`]: [`hir_def::nameres::Unmaintained`],
/// [`hir_def::nameres::FixpointLimit`], [`hir_def::nameres::frozen::Unreadable`]) become the
/// unit's error. Any other panic is not caught.
pub(crate) fn cancellable<T>(work: impl FnOnce() -> T) -> Result<T, Failed> {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    match caught {
        Ok(it) => Ok(it),
        Err(payload) => {
            if payload.is::<blaze_query::db::Cancelled>() || payload.is::<blaze_graph::Cancelled>()
            {
                Err(Failed::Again)
            } else if let Some(it) = hir_def::nameres::stopped(&*payload) {
                Err(Failed::Error(ApiError::new(it, None)))
            } else {
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// One job's front: its units lowered, and what its build scripts and compiles found that
/// later units read.
#[expect(
    clippy::disallowed_types,
    reason = "a job's slots, one per unit, each set once by the unit's task: per job, not global"
)]
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
    /// Each unit's crate as the keys that leave trivia out know it
    /// ([`blaze_lower::Lowered::narrowed`]), set by the writer with its identity: none for a
    /// crate that keeps its whole text.
    pub(crate) narrowed: Box<[OnceLock<Option<blaze_lower::Narrowed>>]>,
    /// The narrowed identities of library crates the writer worked out, the store having none
    /// ([`identify`]), as puts: taken by the next unit that gives the loop its puts
    /// ([`compile::library_puts`]), so a later front end reads them and parses no such crate.
    pub(crate) library_narrows: Lock<Vec<Put>>,
    /// What each build script's run told cargo, by its run unit. Each slot is written once,
    /// when the run ends, and read with no lock by every unit after it.
    pub(crate) scripts: Box<[OnceLock<script::Applied>]>,
    /// What each compiled unit made that later work reads: its main file, its tests. Written
    /// once per unit.
    pub(crate) made: Box<[OnceLock<compile::Made>]>,
    /// What each unit's phases made for the phases after them, by unit.
    pub(crate) phases: Box<[Lock<compile::Phases>]>,
    /// Each unit's record key, worked out once: `fresh` on the loop and `compile` on a worker
    /// both read it.
    pub(crate) keys: Box<[OnceLock<Option<blaze_cas::Key>>]>,
    /// Each unit's narrow record key ([`compile::RecordInputs::key`] of its narrowed identity),
    /// worked out once, as `keys` is.
    pub(crate) narrow_keys: Box<[OnceLock<Option<blaze_cas::Key>>]>,
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
    pub(crate) starts: Box<[Lock<Option<Box<script::Start>>>]>,
    /// What the build scripts' runs found kept read by their bytes ([`script::ReadBytes`]),
    /// noted by `fresh` and read by the writer's `still`, both on the loop: never contended.
    pub(crate) script_reads: Lock<script::ReadBytes>,
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
    later: Lock<Vec<deps::Later>>,
    /// The trees the job's last edit reparsed in place ([`intent::edit`]), kept with the front
    /// end for the next build.
    seeds: Lock<Vec<blaze_intent::Seed>>,
    /// The seeds its front end took: the database holds a seed weakly, so it lives as long as
    /// the job.
    _seeded: Vec<blaze_intent::Seed>,
    /// The code generator's memo, taken once at the job's start: every build of the job reads
    /// and puts in it, and no collection runs while the job holds it.
    pub(crate) memo: Arc<blaze_codegen::memo::Memo>,
    /// Every memo entry each unit's build took or put ([`blaze_codegen::driver::Built::memo_used`]),
    /// by unit, set once at its link; held with its front end once it is kept.
    pub(crate) memo_used: Box<[OnceLock<Vec<ContentId>>]>,
    /// Each `Doctest` unit's examples and runner, made into crates on the loop's writer
    /// ([`compile::doctest::expand`]), by unit.
    pub(crate) doctests: Box<[OnceLock<compile::doctest::Expanded>]>,
    /// What the head's last build used of the memo, kept there until this job's front end is.
    _held: Option<memo::Held>,
}

impl Job {
    fn new(
        input: JobInput,
        crates: Arc<Crates>,
        shared: Arc<Shared>,
        held: Option<memo::Held>,
    ) -> Result<Job, ApiError> {
        let memo = shared.memos.for_job();
        let graph = graph::Graph::new(&input.units)?;
        let config = &input.config;
        let mounted = Arc::new(files::Mounted::new(
            input.files.clone(),
            input.root,
            &config.mounts,
            &config.invocation.root,
        ));
        let slots = graph.len();
        Ok(Job {
            input,
            crates,
            shared,
            graph,
            identities: (0..slots).map(|_| Default::default()).collect(),
            narrowed: (0..slots).map(|_| Default::default()).collect(),
            library_narrows: Lock::new("serve.unit.library_narrows", Vec::new()), // alloc: none
            scripts: (0..slots).map(|_| Default::default()).collect(),
            made: (0..slots).map(|_| Default::default()).collect(),
            phases: (0..slots)
                .map(|_| Lock::new("serve.unit.phases", compile::Phases::default()))
                .collect(),
            keys: (0..slots).map(|_| Default::default()).collect(),
            narrow_keys: (0..slots).map(|_| Default::default()).collect(),
            runs: (0..slots).map(|_| Default::default()).collect(),
            mounted,
            inert: (0..slots).map(|_| Default::default()).collect(),
            pure: (0..slots).map(|_| Default::default()).collect(),
            starts: (0..slots).map(|_| Lock::new("serve.unit.starts", None)).collect(),
            script_reads: Lock::new("serve.unit.script_reads", script::ReadBytes::default()),
            profiles: Default::default(),
            counted: (0..slots).map(|_| Default::default()).collect(),
            lint_profiles: Default::default(),
            layout: Default::default(),
            ub: Default::default(),
            began: Instant::now(),
            later: Lock::new("serve.unit.later", Vec::new()), // alloc: setup, once per job; grows once per dependency crate checked
            seeds: Lock::new("serve.unit.seeds", Vec::new()),
            _seeded: Vec::new(),
            memo,
            memo_used: (0..slots).map(|_| Default::default()).collect(),
            doctests: (0..slots).map(|_| Default::default()).collect(),
            _held: held,
        })
    }

    /// Dependency crate `krate`, checked this build with the lint flags `flags`, whose outcome
    /// `diags` (with whether they break its items) is stored once the front end's images are
    /// ([`deps::store`]); `refused` is why the outcome can't be stored
    /// ([`blaze_diagnostics::cache::encode`]), for the build's report ([`deps::served`]).
    pub(crate) fn check_later(
        &self,
        krate: base_db::Crate,
        flags: Vec<String>,
        diags: Vec<blaze_diagnostics::Diag>,
        items_broken: bool,
        refused: Option<String>,
    ) {
        self.later.lock().push(deps::Later { krate, flags, diags, items_broken, refused });
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

    pub(crate) fn package(
        &self,
        unit: &Unit,
    ) -> Result<&blaze_units::query::PackagePlan, ApiError> {
        package_of(self.units(), unit)
    }

    /// The unit a diagnostic of unit `at` is of, as cargo's JSON names it, with its index in
    /// the request's units.
    pub(crate) fn unit_of(&self, at: usize) -> Result<UnitOf, ApiError> {
        let unit = self.unit(at)?;
        let package = self.package(unit)?;
        let id = u32::try_from(at).map_err(|_| {
            ApiError::new(format!("unit {at} is past the 2^32 units a request can name"), None)
        })?;
        Ok(UnitOf {
            id,
            test: matches!(unit.mode, Mode::Test)
                || (matches!(unit.mode, Mode::Check) && unit.checked_as_test),
            doctest: matches!(unit.mode, Mode::Doctest),
            package_id: unit.pkg_id.clone(),
            manifest_path: package.manifest_path.clone(),
            primary: self.units().is_primary(at),
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
        .ok_or_else(|| ApiError::new(blaze_report::say!("the build has no package ", blaze_report::say::Code(&unit.pkg_id.to_string())).flat(), None))
}

/// A unit's target, as cargo's JSON messages give it. Its kind is cargo's first word for it
/// (any word but `bin`, `test`, `bench`, `example` and `custom-build` is a library's crate
/// type); a crate type blaze doesn't know is an error naming it, as rustc refuses it.
pub(crate) fn unit_target(unit: &Unit) -> Result<UnitTarget, ApiError> {
    let target = &unit.target;
    let Some(first) = target.kind.first() else {
        return Err(ApiError::new(
            blaze_report::say!("target ", blaze_report::say::Code(&target.name), " has no kind in cargo's unit graph").flat(),
            None,
        ));
    };
    let kind = TargetKind::named(first).unwrap_or(TargetKind::Lib);
    let crate_types = target
        .crate_types
        .iter()
        .map(|word| {
            CrateType::named(word).ok_or_else(|| {
                // alloc: error path
                ApiError::new(
                    blaze_report::say!("target ", blaze_report::say::Code(&target.name), " has the crate type ", blaze_report::say::Code(word), ", which rustc doesn't know")
                        .flat(),
                    None,
                )
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
/// but the two whose code the build itself runs, each linked as under `cargo build`: a build
/// script's compile, for its run, and a proc macro's, its wasm32-wasip1 module, for the
/// expansions of the crates that use it ([`codegen::wasm_macro`]). A proc macro only checked
/// has no module, and its `.wasm` output would be a library's crate record (an `ar` archive).
pub(crate) fn checks_only(kind: JobKind, unit: &Unit) -> bool {
    match unit.mode {
        Mode::Check => true,
        Mode::Build if unit.target.is_custom_build() || unit.target.is_proc_macro() => false,
        // A device unit's link is where its MSL is written (`UnitPlan::gpu_msl`), which a
        // checked dependent's front end reads as a built one's does: under every kind of job.
        Mode::Build if unit.for_device() => false,
        _ => kind == JobKind::Check,
    }
}

/// The goal of the request `input` is of, for what its build says of itself
/// ([`blaze_api::Worked::tiers`]): its `--goal`, else the goal of the profile of the first
/// unit it asked for ([`blaze_api::Goal::of_profile`]); none for a request that asked for no
/// unit. The goal one unit's code is compiled for is [`codegen::goal`]'s to say.
pub(crate) fn request_goal(input: &JobInput) -> Option<blaze_api::Goal> {
    input.goal.or_else(|| {
        let graph = &input.units.graph;
        let profile = &graph.units.get(*graph.roots.first()?)?.profile;
        Some(blaze_api::Goal::of_profile(
            &profile.name,
            &profile.opt_level,
            profile.codegen_backend.as_deref(),
        ))
    })
}

/// The phases of `unit`'s compile, each a node of the plane ([`Phase`]): no codegen and no
/// link for a unit [`checks_only`] stops after the front end.
pub(crate) fn phases(kind: JobKind, unit: &Unit) -> &'static [Phase] {
    match checks_only(kind, unit) {
        true => &Phase::CHECK,
        false => &Phase::BUILD,
    }
}

/// The key phase `phase` (its codegen or its link) of a program's build is predicted and
/// taught by (`plane::Plane::push_program`), when `unit` builds a program its client runs: a
/// binary, test, bench, example or `cdylib`. A build script and a proc macro are not: many
/// and small, they are admitted as any compile. The same program of the same profile and
/// platform has the same key in every build and daemon. `asked`: [`codegen::asked`].
pub(crate) fn program_key(unit: &Unit, asked: bool, phase: Phase) -> Option<blaze_graph::Key> {
    // A wasm proc macro's units run in the macro host, never as a program of the client's.
    if unit.target.is_custom_build()
        || unit.macro_wasm
        || !matches!(unit.mode, Mode::Build | Mode::Test)
    {
        return None;
    }
    if !matches!(
        codegen::makes(unit, asked),
        Ok(codegen::Makes::Executable | codegen::Makes::Shared { proc_macro: false })
    ) {
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

/// The key phase `phase` of `unit` (`None` for a build script's run) is predicted and taught
/// by (`plane::Plane::push_learned`), where it is not a program's ([`program_key`]): the same
/// unit of the same profile, platform and mode has the same key in every build and daemon, so
/// its node costs what its own last runs that did work took, which the daemon's image keeps: a
/// run of a fresh unit replays its record and teaches nothing (`plane::Plane::replays`). Its
/// memory is never under its kind's predicted peak (`plane::Plane::push_learned`).
pub(crate) fn cost_key(unit: &Unit, phase: Option<Phase>) -> blaze_graph::Key {
    let mut hasher = blaze_ids::Hasher::new("blaze v2 unit cost 1");
    let platform = unit.platform.as_deref().unwrap_or("");
    let parts = [unit.pkg_id.as_str(), &unit.target.name, &unit.profile.name, platform];
    for part in parts.into_iter().chain(unit.target.kind.iter().map(String::as_str)) {
        hasher.update(&(part.len() as u64).to_le_bytes()).update(part.as_bytes());
    }
    hasher.update(&[
        unit.mode as u8,
        u8::from(unit.macro_wasm),
        phase.map_or(u8::MAX, |it| it as u8),
    ]);
    blaze_graph::Key(hasher.finish().0)
}

/// The most directory entries [`source_bytes`] reads for one unit (rule 18): a unit rooted
/// in a directory of more (a package root holding a vendored tree) is sized by what it read.
const SOURCE_ENTRIES: usize = 50_000;

/// The bytes of Rust source of `unit`'s target, what its phases' memory is predicted from
/// (`plane::Plane::push_sized`): every `.rs` file where its modules live by Rust's rule. A
/// root named `lib.rs`, `main.rs` or `mod.rs` keeps its modules beside it, so its whole
/// directory; any other (`build.rs`, `tests/it.rs`, `src/bin/tool.rs`) itself and the
/// directory of its stem (`tests/it/`). Hidden directories and `target` are not read. A
/// prediction only, never an input of what is built: a directory that can't be read counts
/// 0, and 0 in all predicts the unit as [`plane::Plane::push_learned`] does. `seen` keeps each
/// directory's count for the build, so a package's many units read it once.
pub(crate) fn source_bytes(
    unit: &Unit,
    seen: &mut blaze_hash::HashMap<PathBuf, u64, std::alloc::Global>,
) -> u64 {
    let root = unit.target.src_path.as_path();
    let Some(dir) = root.parent() else { return 0 };
    let modules_beside = matches!(
        root.file_name().and_then(|it| it.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    );
    if modules_beside {
        return dir_source_bytes(dir, seen);
    }
    let own = std::fs::metadata(root).map_or(0, |it| it.len());
    // alloc: the stem's directory's path, once a unit a build
    let below = root.file_stem().map_or(0, |stem| dir_source_bytes(&dir.join(stem), seen));
    own + below
}

/// The bytes of every `.rs` file under `dir`, at most [`SOURCE_ENTRIES`] entries read; once a
/// build per directory, through `seen`.
fn dir_source_bytes(
    dir: &std::path::Path,
    seen: &mut blaze_hash::HashMap<PathBuf, u64, std::alloc::Global>,
) -> u64 {
    if let Some(&bytes) = seen.get(dir) {
        return bytes;
    }
    let mut bytes = 0u64;
    let mut entries = 0usize;
    let mut stack = vec![dir.to_path_buf()]; // alloc: a walk's directories, once a directory a build
    'walk: while let Some(at) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&at) else { continue };
        for entry in read.flatten() {
            entries += 1;
            if entries > SOURCE_ENTRIES {
                break 'walk;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(entry.path()); // alloc: a directory to read, once a build
                }
            } else if kind.is_file() && name.ends_with(".rs") {
                bytes += entry.metadata().map_or(0, |it| it.len());
            }
        }
    }
    seen.insert(dir.to_path_buf(), bytes); // alloc: the directory's count, kept for the build
    bytes
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

    fn compile(
        &self,
        reader: UnitReader,
        unit: usize,
        phase: Phase,
        images: &mut Vec<Image<Real>>,
    ) -> Result<Option<Compiled>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| compile::compile(&self.0, &reader, unit, phase, images))?
    }

    fn stand(
        &self,
        reader: UnitReader,
        unit: usize,
        images: &mut Vec<Image<Real>>,
    ) -> Result<Option<Compiled>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| compile::stand(&self.0, &reader, unit, images))?
    }

    fn sealed(&self) -> Result<Vec<bool>, ApiError> {
        let units = self.0.units();
        // What pins each package, as the lowering wrote it on its crates.
        let pins = compile::pins(units, &self.0.input.config.sources)?;
        let by_id: HashMap<&str, bool> = (units.packages.iter())
            .zip(&pins)
            .map(|(package, pin)| (package.id.as_str(), pin.is_some()))
            .collect(); // alloc: once per build that a stood plan applies to, a package an entry
        // A unit of no package of the plan is no unit the planner sealed.
        let sealed: Vec<bool> = (units.graph.units.iter())
            .map(|unit| by_id.get(unit.pkg_id.as_str()).copied().unwrap_or(false))
            .collect(); // alloc: once per build that a stood plan applies to, a flag a unit
        Ok(sealed)
    }

    fn refused(&self, unit: usize, what: String) -> blaze_api::Diagnostic {
        say::refused(what, self.0.unit_of(unit).ok(), compile::drawing(&self.0).color)
    }

    fn subject(&self, unit: usize) -> Option<(String, String)> {
        let of = self.0.unit(unit).ok()?;
        // alloc: a task span's names, once a phase task
        Some((of.target.name.clone(), compile::span_unit(of)))
    }

    fn script(&self, unit: usize, tags: u64) -> Result<UnitRuns, ApiError> {
        script::start(&self.0, unit, tags).map(UnitRuns::Script)
    }

    fn tests(
        &self,
        built: &[Artifact],
        selection: Selection<'_>,
        tags: u64,
    ) -> Result<UnitRuns, ApiError> {
        test::start(&self.0, built, selection, tags).map(UnitRuns::Tests)
    }

    fn ub_tests(
        &self,
        reader: UnitReader,
        selection: Selection<'_>,
    ) -> Result<Vec<String>, Failed> {
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

    fn edit(
        &self,
        reader: UnitReader,
        edits: &[blaze_api::Edit],
    ) -> Result<blaze_api::Edited, Failed> {
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

    fn prove(&self, reader: UnitReader, emit: bool, subset: bool) -> Result<prove::Proving, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| prove::prove(&self.0, reader.db(), emit, subset))?
    }

    fn codegen_inputs(&self, reader: UnitReader) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        // Read from the units' records in the store: the front end is not asked.
        drop(reader);
        cancellable(|| compile::codegen_inputs(&self.0))?
    }

    fn tables(
        &self,
        mut writer: UnitWriter,
        images: &[(&'static str, u64, u64)],
    ) -> Result<Vec<Table>, Failed> {
        let mut counted = writer.tables()?;
        counted.extend_from_slice(images);
        Ok(summed(counted))
    }

    fn doc(
        &self,
        mut writer: UnitWriter,
        no_deps: bool,
        private: bool,
    ) -> Result<Vec<Response>, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        if !writer.apply()? {
            let what =
                "the documentation is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        }
        let Some(lowered) = Arc::get_mut(&mut writer.front) else {
            let what =
                "the documentation is made with no task reading the front end, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| doc::doc(&self.0, lowered, no_deps, private))?.map_err(Failed::Error)
    }
}

impl Kept for codegen::Linked {
    fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        codegen::Linked::tables(self)
    }

    fn held(&self) -> u64 {
        codegen::Linked::held(self)
    }

    fn live(&self) -> bool {
        codegen::Linked::live(self)
    }

    fn rest(&mut self) {
        codegen::Linked::rest(self);
    }
}

/// User code of a job, run on the loop: a build script's run, the build's tests, its
/// program, or Lean over its proofs.
pub enum UnitRuns {
    Script(script::ScriptRun),
    Tests(test::TestRun),
    Program(run::Program),
    Proofs(prove::Checking),
}

impl Runs<Real> for UnitRuns {
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.start(store, writer, effects, out),
            UnitRuns::Tests(it) => it.start(store, writer, effects, out),
            UnitRuns::Program(it) => it.start(store, writer, effects, out),
            UnitRuns::Proofs(it) => it.start(store, writer, effects, out),
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        match self {
            UnitRuns::Script(it) => it.owns(tag),
            UnitRuns::Tests(it) => it.owns(tag),
            UnitRuns::Program(it) => it.owns(tag),
            UnitRuns::Proofs(it) => it.owns(tag),
        }
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.complete(completion, store, writer, effects, out),
            UnitRuns::Tests(it) => it.complete(completion, store, writer, effects, out),
            UnitRuns::Program(it) => it.complete(completion, store, writer, effects, out),
            UnitRuns::Proofs(it) => it.complete(completion, store, writer, effects, out),
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self {
            UnitRuns::Script(it) => it.cancel(effects),
            UnitRuns::Tests(it) => it.cancel(effects),
            UnitRuns::Program(it) => it.cancel(effects),
            UnitRuns::Proofs(it) => it.cancel(effects),
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        match self {
            UnitRuns::Script(it) => it.take_puts(),
            UnitRuns::Tests(it) => it.take_puts(),
            UnitRuns::Program(it) => it.take_puts(),
            UnitRuns::Proofs(it) => it.take_puts(),
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
