//! [`build`] is `build` and `run` (their one program: the units settled, its roots listed, the
//! program walked, coded, linked and placed through [`crate::images::HomeImages`]); [`start`]
//! is `run`'s exec after it. What follows is of `check`, which `build` shares but for that.
//!
//! One `check` request: the graph and a front end in, the host run on the store under
//! `<home>/v2/keyed`, what it did out ([`Summary`]). The thin driver of the command: it reads
//! the kept files, sweeps the checkout, builds the [`Plan`] and the [`Request`], runs
//! [`blaze_host::run`] and keeps what the next command starts from. No decision about what to
//! build is made here: the driver decides, from the certificate down.
//!
//! What is built, and from what:
//!
//! - the plan: the certificate's key ([`Certificate::key`] of the tool, the target, the
//!   namespace and the graph's identity as its base: the driver widens it with the units whose
//!   bodies are not asked, [`blaze_keyed::driver::certificate_of`]), and per unit its
//!   dependencies, owner, namespace, whether its bodies are asked, and `last`, its manifest
//!   when the last command ended ([`Answered::manifests`], kept per crate key);
//! - the certificate's look ([`look_up`]): the driver's own lookup of [`certificate_of`] the plan,
//!   run before any text is read, over the sweep's answers. When it holds, nothing is gathered
//!   and every late table is empty;
//! - the text, only when the look missed: every unit's gathered by the front end before the
//!   request ([`Gathers::gather`]) through [`DiskSource`], which holds each checkout file to
//!   the answer the request hands over for it;
//! - the request: the reads and their answers from the leaf memory ([`leaves::Reads`]), one
//!   late table per unit (the front end's, [`Gathers::late`]), the leaves, and [`STALL`];
//! - the store: blaze-cas's plain store at `<home>/v2/keyed/store`; the memo
//!   [`RecordsOnly`]; the machine as the caller read it.
//!
//! What is kept, per checkout (`<home>/v2/keyed/{leaves,last}/<checkout id>`): the leaf table
//! and the manifests, each written whole or not at all, and only when it moved.
//!
//! Floors (rule 67): a no-op's is the sweep (its stats, see [`crate::leaves`]) and one
//! certificate lookup (the shape slot read, one answer per read, which the fresh leaf memory
//! answers without a hash, and the value read). A no-op here is the sweep, one certificate
//! lookup ([`look_up`], in the request's snapshot, which `blaze_host::run` takes as its own:
//! the driver checks its key and that it is done, and reads no slot again), opening the store
//! and reading two small kept files, beside the graph's load (see [`crate::graph`]), which no
//! graph record skips yet. It gathers nothing: no file of any unit is read, hashed or imaged.
//! Against the floor: 1x the lookup; the gap is the store's open and the two kept files (each
//! one small read), and the graph's load.
//!
//! On a miss every unit is gathered before the request: the driver keys every unit on a miss
//! and takes each unit's late table up front ([`Request::late`]), and a surface or body task
//! needs the text of its whole closure. What gathers only the moved units is the driver's: key
//! a unit whose files did not move from its kept manifest, its late table from the manifest.
//! The look's answer is the request's: a look that held certifies the request, so a file that
//! moves between the look and the request's end is the driver's race to name
//! (`Failure::Raced`), never a key task without text. An edit's floor is the driver's; this adds nothing per task but one relaxed atomic
//! add per task ([`Counting`]).

use std::{
    alloc::{Allocator, Global},
    fmt,
    io::Read as _,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use blaze_cas::plain::{Form, PlainStore, Settings as StoreSettings};
use blaze_codegen::{
    plan::{Mode as CodeMode, Policy},
    settings::{DebugInfo, Panic, Pgo, Sanitize},
};
use blaze_format::Aligned;
use blaze_host::{
    Answered, Request, UnitProved,
    disk::Disk,
    outside::{Answerer, Ask, Files, Leaves, file_answer},
    run, run_programs,
};
use blaze_ids::{ContentId, Hasher, crc32c::crc32c};
use blaze_keyed::{
    driver::{Asks, Bodies, CertificateLookup, Failure, ItemPlace, Outcome, Plan, TaskFailed, UnitPlan, certificate_of, certificate_with},
    front::{Front, GivenBody, Keyed, Linked, SpanTables, Table, Universe},
    kind::{Body, Certificate, CertificateInputs, CodeRecord, Key, Kind, Namespace, PlanRecord, Steps, Surface, ToolId, UnitManifest},
    machine::Machine,
    memo::RecordsOnly,
    outside::{Found, Lookup, Want},
    program::{Kept, Planned, ProgramPlan, RowChange, WalkAsk, WalkEnd, Walker},
    proof::{self, Checks, Snapshot},
    record::{self, Fact, StableId},
    store::{Store, StoreError},
};
use blaze_program::{
    key::{DebugInfo as WalkDebugInfo, Panic as WalkPanic, Settings as WalkSettings, Tier},
    walker::ProgramWalker,
};

use crate::{
    CommandError,
    front::{Linking, Source, SourceError},
    graph::{Graph, Program, Unit},
    leaves::{self, Cost, DiskLook, Reads},
    program,
};

/// The most one wait for a task may take with none finishing before the request stops
/// ([`Request::stall`]): far past any one body's check, so only a hung task reaches it.
pub const STALL: Duration = Duration::from_secs(300);

/// What one unit's request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct UnitDone {
    /// Tasks run, by kind.
    pub keyed: u32,
    pub surfaces: u32,
    pub bodies: u32,
    /// Tasks that failed (refused or panicked).
    pub failed: u32,
    /// Records proved at the end, by kind: its manifest, its surface, its bodies.
    pub manifest: bool,
    pub surface: u32,
    pub body: u32,
}

/// How long each phase of [`check`] took.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Phases {
    /// Reading the kept files and sweeping the checkout.
    pub sweep: Duration,
    /// The request's reads from the sweep, opening the store, and the plan.
    pub plan: Duration,
    /// The certificate's look before any text is read ([`look_up`]).
    pub look: Duration,
    /// Gathering every unit's text ([`Gathers::gather`]); nothing when the look held.
    pub gather: Duration,
    /// The request's late tables, and the host's run.
    pub host: Duration,
    /// Writing the kept files.
    pub keep: Duration,
}

/// What a request did.
pub struct Summary<A: Allocator + Clone> {
    pub outcome: Outcome,
    pub asks: Asks,
    /// Per unit, in the graph's order.
    pub units: Vec<UnitDone, A>,
    pub phases: Phases,
    pub sweep: Cost,
    /// Reads the request had.
    pub reads: u32,
    /// Whether any unit's text was gathered: `false` when the certificate's look held.
    pub gathered: bool,
    /// The store's own error behind a store failure, when there was one.
    pub store_error: Option<StoreError>,
    /// Every failure of a failed request, in the driver's order (the outcome's `why` first):
    /// the driver goes on past a failed task, so each is printed under its unit.
    pub failures: Vec<Failure, Global>,
    /// What refused tasks said beyond their rules, one line each with the unit it is about,
    /// printed under that unit ([`Gathers::told`]).
    pub told: Vec<(u32, Box<str, A>), A>,
    /// Each item place a failure names ([`TaskFailed::TwoItems`]), by its unit, as a person
    /// reads it ([`Gathers::place`]): what the summary prints at `file:line`.
    pub places: Vec<(u32, ItemPlace, Box<str, A>), A>,
}

impl<A: Allocator + Clone> Summary<A> {
    /// Place `at` of unit `unit` as [`Summary::places`] holds it; `None` for a place no
    /// failure named.
    pub fn place(&self, unit: u32, at: ItemPlace) -> Option<&str> {
        self.places.iter().find(|(of, place, _)| *of == unit && *place == at).map(|(_, _, shown)| &**shown)
    }
}

/// What the command needs of a front end beside [`Front`]: each unit's text gathered before
/// the request, through a [`Source`] (so the front end reads no disk), each unit's late table,
/// and what its refused tasks said beyond their rules. [`crate::front::Command`] is the real
/// one; a test may give its own.
pub trait Gathers: Front {
    /// Gathers unit `unit`'s text through `source`; called for every unit, in the graph's
    /// order, before the request, unless the certificate's look held.
    fn gather<S: Source>(&mut self, unit: u32, source: &mut S) -> Result<(), CommandError>;
    /// Unit `unit`'s late table, sorted by id, each once: [`Request::late`]'s row; empty for a
    /// unit not gathered.
    fn late(&self, unit: u32) -> &[Fact];
    /// What refused tasks said beyond their rules (each item at its file and line), one line
    /// each with the unit it is about, in `alloc`.
    fn told<B: Allocator + Clone>(&self, alloc: B) -> Vec<(u32, Box<str, B>), B>;
    /// Item place `at` of unit `unit` as a person reads it (`path:line`), in `alloc`.
    fn place<B: Allocator + Clone>(&self, unit: u32, at: ItemPlace, alloc: B) -> Box<str, B>;
}

impl<A: Allocator + Clone + Send + Sync> Gathers for crate::front::Command<'_, A> {
    fn gather<S: Source>(&mut self, unit: u32, source: &mut S) -> Result<(), CommandError> {
        crate::front::Command::gather(self, unit, source)
    }

    fn late(&self, unit: u32) -> &[Fact] {
        crate::front::Command::late(self, unit)
    }

    fn told<B: Allocator + Clone>(&self, alloc: B) -> Vec<(u32, Box<str, B>), B> {
        crate::front::Command::told(self, alloc)
    }

    fn place<B: Allocator + Clone>(&self, unit: u32, at: ItemPlace, alloc: B) -> Box<str, B> {
        crate::front::Command::place(self, unit, at, alloc)
    }
}

/// Every unit the command cannot build yet, one per line with why: the error that stops the
/// command before anything is read; `None` when there is none.
pub fn blocked<A: Allocator + Clone>(graph: &Graph<A>) -> Option<CommandError> {
    let mut count = 0usize;
    // The error's lifetime: the global heap, as every `CommandError` text is.
    let mut units = blaze_mem::String::new_in(Global);
    for unit in graph.units.iter() {
        if let Some(why) = unit.blocked {
            count += 1;
            blaze_mem::text::format_into(&mut units, format_args!("  {}: {why}\n", named(unit)));
        }
    }
    match count {
        0 => None,
        _ => Some(CommandError::NotBuiltYet { count, units: units.into_boxed_str() }),
    }
}

/// Runs `check` on `graph` with `front`, filing under `<home>/v2/keyed`.
///
/// A unit the command cannot build yet stops it before anything is read, every such unit named
/// ([`blocked`], [`crate::NOT_BUILT_YET`]).
pub fn check<F: Gathers, A: Allocator + Clone>(
    graph: &Graph<A>,
    front: &mut F,
    home: &Path,
    machine: Machine,
    alloc: A,
) -> Result<Summary<A>, CommandError> {
    if let Some(error) = blocked(graph)
        && !planted(Plant::IgnoresBlocked)
    {
        return Err(error);
    }
    let keyed = home.join("v2").join("keyed");
    let place = place_of(&graph.checkout);
    let leaves_at = keyed.join("leaves").join(&place);
    let last_at = keyed.join("last").join(&place);

    let began = Instant::now();
    let mut look = DiskLook::new(Path::new(&*graph.checkout));
    let fresh = leaves::sweep(&mut look, leaves::load(&leaves_at)?)?;
    let mut table = fresh.table.clone();
    let moved = fresh.moved();
    let last = load_last(&last_at, alloc.clone())?;
    let sweep_took = began.elapsed();

    let began = Instant::now();
    let mut reads = Reads::of(&fresh.table, alloc.clone())?;
    if planted(Plant::SkipsARead) {
        reads.asks.pop();
        reads.then.pop();
        reads.leaves.pop();
    }
    let store = open_store(&keyed)?;
    let mut deps = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for unit in graph.units.iter() {
        let mut list = Vec::with_capacity_in(unit.deps.len(), alloc.clone());
        list.extend(unit.deps.iter().map(|dep| dep.unit));
        deps.push(list);
    }
    let mut plans = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for (unit, deps) in graph.units.iter().zip(deps.iter()) {
        plans.push(UnitPlan {
            deps,
            last: last.iter().find(|it| it.0 == unit.key).map(|it| Key::from_id(it.1)),
            namespace: unit.namespace,
            owner: unit.owner,
            bodies: unit.bodies,
        });
    }
    // The certificate is the checkout's: filed where its members are.
    let namespace = graph.units.iter().find(|unit| unit.member).map_or(Namespace::LOCAL, |unit| unit.namespace);
    let base = match planted(Plant::BaseOfRun) {
        true => Hasher::new("blaze-command planted base").id(&graph.identity).update(&table.instant_ns.to_le_bytes()).finish(),
        false => graph.identity,
    };
    let tool = ToolId { tool: graph.tool, target: graph.target, namespace };
    let certificate = Certificate::key(&CertificateInputs { tool, base, of: PhantomData });
    let plan = Plan { certificate, namespace, units: &plans };
    let files = Disk::new(Path::new(&*graph.checkout));
    let plan_took = began.elapsed();

    // A typed refusal a task unwinds with is told under its unit, not printed as a crash.
    let _quiet = quiet_refusals();
    // One snapshot for the look and the request: the look's `Lookup` is the request's own
    // (`blaze_host::run`'s `certificate_lookup`), so the certificate is looked up once.
    let ran = proof::scope(|snap| {
        // The certificate looked up before any text is read: a hit gathers nothing.
        let began = Instant::now();
        let looked = match planted(Plant::NoLook) {
            true => None,
            false => Some(look_up(snap, certificate_of(&plan), &store, &files, reads.leaves())?),
        };
        let held = looked.as_ref().is_some_and(|lookup| matches!(lookup.found(), Some(Found::Hit(_))));
        let look_took = began.elapsed();

        let began = Instant::now();
        if !held {
            let mut source = DiskSource { checkout: &graph.checkout, asks: &reads.asks, then: &reads.then, buffer: [0; READ_CHUNK] };
            for unit in 0..graph.units.len() {
                let _span = tracing::info_span!(target: "blaze", "command: gathering a unit's text", unit).entered();
                front.gather(u32::try_from(unit).unwrap_or(u32::MAX), &mut source)?;
            }
        }
        let gather_took = began.elapsed();
        let front: &F = front;

        let began = Instant::now();
        // Each unit's late table: empty for every unit when the look held (none was gathered),
        // which a certified request never reads.
        let mut late_refs = Vec::with_capacity_in(graph.units.len(), alloc.clone());
        late_refs.extend((0..graph.units.len()).map(|unit| front.late(u32::try_from(unit).unwrap_or(u32::MAX))));
        let request = Request { plan, reads: &reads.asks, then: &reads.then, late: &late_refs, leaves: reads.leaves(), stall: STALL };
        let counting = Counting::new(front, graph.units.len());
        let _span = tracing::info_span!(target: "blaze", "command: the keyed request", units = graph.units.len()).entered();
        let answered = run(snap, &request, looked, &store, &counting, &mut RecordsOnly, &files, machine)?;
        let mut units = Vec::with_capacity_in(graph.units.len(), alloc.clone());
        for at in 0..graph.units.len() {
            let tally = &counting.tally[at];
            let mut done = UnitDone {
                keyed: tally.keyed.load(Ordering::Relaxed),
                surfaces: tally.surfaces.load(Ordering::Relaxed),
                bodies: tally.bodies.load(Ordering::Relaxed),
                failed: tally.failed.load(Ordering::Relaxed),
                ..UnitDone::default()
            };
            if let Some(proved) = answered.units.get(at) {
                done.manifest = proved.manifest.is_some();
                for (_, holds) in &proved.records {
                    match holds.tag() {
                        tag if tag == Surface::TAG => done.surface += 1,
                        tag if tag == Body::TAG => done.body += 1,
                        _ => {}
                    }
                }
            }
            units.push(done);
        }
        let mut manifests = Vec::with_capacity_in(graph.units.len(), alloc.clone());
        for (unit, manifest) in graph.units.iter().zip(answered.manifests.iter()) {
            if let Some(manifest) = manifest {
                manifests.push((unit.key, manifest.id()));
            }
        }
        leaves::absorb(&mut table, &answered.looked);
        let host_took = began.elapsed();
        Ok::<_, CommandError>(Requested {
            outcome: answered.outcome,
            asks: answered.asks,
            units,
            manifests,
            programs: Vec::new_in(alloc.clone()),
            failures: answered.failures,
            store_error: answered.store_error,
            looked: !answered.looked.is_empty(),
            held,
            look_took,
            gather_took,
            host_took,
        })
    })?;
    let Requested { outcome, asks, units, mut manifests, programs: _, failures, store_error, looked, held, look_took, gather_took, host_took } = ran;

    let began = Instant::now();
    if moved || looked {
        leaves::save(&leaves_at, &table)?;
    }
    manifests.sort();
    if manifests[..] != last[..] {
        save_last(&last_at, &manifests, alloc.clone())?;
    }
    let keep_took = began.elapsed();
    let mut places = Vec::new_in(alloc.clone());
    for failure in failures.iter() {
        if let Failure::Task { unit, failed: TaskFailed::TwoItems { first, second }, .. } = *failure {
            for at in [first, second] {
                places.push((unit, at, front.place(unit, at, alloc.clone())));
            }
        }
    }
    Ok(Summary {
        outcome,
        asks,
        units,
        phases: Phases {
            sweep: sweep_took,
            plan: plan_took,
            look: look_took,
            gather: gather_took,
            host: host_took,
            keep: keep_took,
        },
        sweep: fresh.cost,
        reads: u32::try_from(reads.asks.len()).unwrap_or(u32::MAX),
        gathered: !held,
        store_error,
        failures,
        told: front.told(alloc),
        places,
    })
}

// --- build and run: one program ------------------------------------------------------------------

/// How far one program's walk may go ([`ProgramWalker::new`]'s fuel): past any real program's
/// instances many times over, so only a walk that does not end reaches it (rule 18).
pub const WALK_FUEL: u64 = 1 << 40;

/// `build` or `run` found no program to link.
pub const NO_BINARY: &str = "`build` and `run` link one program, and no workspace member has a binary target built in this mode";
/// `build` or `run` found more than one program.
pub const TWO_BINARIES: &str =
    "`build` and `run` link one program, and more than one workspace member binary is built (`--bin` to pick one is not here yet)";
/// The code generator's toolchain for the program's target did not resolve.
pub const NO_PROGRAM_TOOLCHAIN: &str =
    "a program's toolchain: the target's data or the rustflags did not resolve (`hir_ty_codegen::v2::toolchain`)";
/// The linker has no output for the target.
pub const NO_LINK_OUTPUT: &str = "a program's link: the linker has no output for the target (`blaze_link::target::output`)";
/// The program's name is no file name.
pub const NOT_A_FILE_NAME: &str = "a program's file: its name is not one file name under the output directory (`blaze_core::FileName`)";
/// The output path is not UTF-8.
pub const PATH_NOT_UTF8: &str = "a program's file: the output directory is not UTF-8, which the command keeps its path as";
/// A unit of the program's closure has no surface to list the roots over.
pub const NO_CLOSURE_SURFACE: &str = "a program's roots: a unit of its closure has no manifest after the units settled, or its manifest or surface does not open under its key";
/// `run`'s exec did not start the program.
pub const NOT_STARTED: &str = "`run`: the program did not start (the exec failed)";

/// What [`build`] did: the request's summary, the program's path when the answer placed it
/// (built or certified): what `run` starts, and what the images said beyond it (each copy
/// made where the file system shares no blocks, [`crate::images::HomeImages::notes`]).
pub struct Built<A: Allocator + Clone> {
    pub summary: Summary<A>,
    pub program: Option<PathBuf>,
    pub notes: Vec<Box<str, Global>, Global>,
}

/// Runs `build` (and `run`'s build) on `graph`: its one program ([`Graph::programs`]) walked
/// from its roots, coded and linked into `<out_dir>/<name>`, the units under it settled with
/// their bodies demanded by the walk alone (rule: `run` and `build` check only the code the
/// program reaches), filing under `<home>/v2/keyed`.
///
/// The order: the certificate of the units and the program ([`certificate_with`]) looked up
/// before any text is read. Held: nothing is gathered, walked or linked; the answer places the
/// path from the store all the same ([`crate::images::HomeImages`]: one look, a link only when
/// it names another image or nothing). Missed: every unit's text gathered, then one request
/// ([`run_programs`]): the units settle, each keyed once; the walk begins and lists the
/// program's roots over its closure's surfaces as they settled ([`Rooted`], [`program::roots`]);
/// then the plans, the code and the link step, whose patch the host makes into the image named
/// by the kept manifest's key, then the place.
///
/// Floor (rule 67): a no-op's is [`check`]'s (the sweep and one certificate lookup) and one look
/// at the program's path, and it is that (the look is one stamp of the path and one read of its
/// placed record, [`crate::images`]). An edit's is the driver's: the moved bodies, their plans and code, one link step. Gap
/// against it: the roots listed again every miss (one manifest and surface read per unit of the
/// closure, and the front end's roots pass: not a record yet, see [`program::roots`]).
pub fn build<A: Allocator + Clone + Send + Sync>(
    graph: &Graph<A>,
    front: &mut crate::front::Command<'_, A>,
    home: &Path,
    out_dir: &Path,
    machine: Machine,
    alloc: A,
) -> Result<Built<A>, CommandError> {
    if let Some(error) = blocked(graph) {
        return Err(error);
    }
    let program = one_program(graph)?;
    let unit = &graph.units[program.unit as usize];
    let (linking, path) = linking_of(graph, front, program, out_dir, alloc.clone())?;
    let policy = linking.policy.clone();
    let settings = linking.settings;
    // One snapshot kept per worker: a plan task leases one ([`crate::snapshot::lease_plan`]).
    let number = front.add_program(linking, machine.cores as usize);
    debug_assert_eq!(number, 0, "the walker numbers the one program 0");

    let keyed = home.join("v2").join("keyed");
    let place = place_of(&graph.checkout);
    let leaves_at = keyed.join("leaves").join(&place);
    // Not `check`'s file: a build's units are filed with their bodies not asked, under other
    // manifest keys (`driver::manifest_of`), and the two would overwrite each other's hints.
    let last_at = keyed.join("last-build").join(&place);
    let programs_at = keyed.join("programs").join(&place);

    let began = Instant::now();
    let mut look = DiskLook::new(Path::new(&*graph.checkout));
    let fresh = leaves::sweep(&mut look, leaves::load(&leaves_at)?)?;
    let mut table = fresh.table.clone();
    let moved = fresh.moved();
    let last = load_last(&last_at, alloc.clone())?;
    let last_programs = load_last(&programs_at, alloc.clone())?;
    let sweep_took = began.elapsed();

    let began = Instant::now();
    let reads = Reads::of(&fresh.table, alloc.clone())?;
    let store = open_store(&keyed)?;
    let mut deps = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for unit in graph.units.iter() {
        let mut list = Vec::with_capacity_in(unit.deps.len(), alloc.clone());
        list.extend(unit.deps.iter().map(|dep| dep.unit));
        deps.push(list);
    }
    let mut plans = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for (unit, deps) in graph.units.iter().zip(deps.iter()) {
        plans.push(UnitPlan {
            deps,
            last: last.iter().find(|it| it.0 == unit.key).map(|it| Key::from_id(it.1)),
            namespace: unit.namespace,
            owner: unit.owner,
            // Only what the program reaches is checked: every body is demanded by a plan.
            bodies: Bodies::NotAsked,
        });
    }
    let namespace = unit.namespace;
    let tool = ToolId { tool: graph.tool, target: graph.target, namespace };
    let certificate = Certificate::key(&CertificateInputs { tool, base: graph.identity, of: PhantomData });
    let identity = Hasher::new("blaze-command program 1")
        .id(&unit.key)
        .part(program.name.as_bytes())
        .id(&graph.target)
        .id(&policy.id())
        .finish();
    let programs = [ProgramPlan {
        program: identity,
        settings: policy.id(),
        namespace,
        last: last_programs.iter().find(|it| it.0 == identity).map(|it| Key::from_id(it.1)),
    }];
    let files = Disk::new(Path::new(&*graph.checkout));
    let images = crate::images::HomeImages::new(&keyed, &[(identity, path.clone())], Global)?;
    let plan_took = began.elapsed();

    let _quiet = quiet_refusals();
    let ran = proof::scope(|snap| {
        let plan = Plan { certificate, namespace, units: &plans };
        let began = Instant::now();
        let looked = look_up(snap, certificate_with(&plan, &programs), &store, &files, reads.leaves())?;
        let held = matches!(looked.found(), Some(Found::Hit(_)));
        let look_took = began.elapsed();

        let began = Instant::now();
        if !held {
            let mut source = DiskSource { checkout: &graph.checkout, asks: &reads.asks, then: &reads.then, buffer: [0; READ_CHUNK] };
            for at in 0..graph.units.len() {
                let _span = tracing::info_span!(target: "blaze", "command: gathering a unit's text", unit = at).entered();
                front.gather(u32::try_from(at).unwrap_or(u32::MAX), &mut source)?;
            }
        }
        let gather_took = began.elapsed();
        let front: &crate::front::Command<'_, A> = front;

        let began = Instant::now();
        let mut late_refs = Vec::with_capacity_in(graph.units.len(), alloc.clone());
        late_refs.extend((0..graph.units.len()).map(|at| front.late(u32::try_from(at).unwrap_or(u32::MAX))));
        let counting = Counting::new(front, graph.units.len());
        let mut inner =
            ProgramWalker::new(program::Reader { policy: &policy, tool: ToolId { tool: graph.tool, target: graph.target, namespace } }, walk_settings(&policy), WALK_FUEL, blaze_program::Plant::None);
        if held {
            // Certified: the walk never starts, and the driver takes the program's manifest from
            // the certificate.
            inner.add_program(&[]);
        }
        // One request: the units settle, then the walk begins and lists its roots over them.
        let mut refused = None;
        let walker = Rooted { inner, graph, front, program, policy: &policy, settings, store: &store, listed: held, failed: &mut refused };
        let request = Request { plan, reads: &reads.asks, then: &reads.then, late: &late_refs, leaves: reads.leaves(), stall: STALL };
        let answered = {
            let _span = tracing::info_span!(target: "blaze", "command: the program's request", program = &*program.name).entered();
            run_programs(snap, &request, Some(looked), &programs, walker, &store, &counting, &mut RecordsOnly, &files, &images, machine)?
        };
        if let Some(error) = refused {
            return Err(error);
        }
        leaves::absorb(&mut table, &answered.looked);
        let mut kept = Vec::with_capacity_in(1, alloc.clone());
        if let Some(Some(manifest)) = answered.programs.first() {
            kept.push((identity, manifest.id()));
        }
        let host_took = began.elapsed();
        Ok(Requested {
            programs: kept,
            held,
            look_took,
            gather_took,
            host_took,
            ..requested_of(graph, &counting, answered, alloc.clone())
        })
    })?;
    let Requested { outcome, asks, units, mut manifests, programs: kept, failures, store_error, looked, held, look_took, gather_took, host_took } = ran;

    let began = Instant::now();
    if moved || looked {
        leaves::save(&leaves_at, &table)?;
    }
    manifests.sort();
    if manifests[..] != last[..] {
        save_last(&last_at, &manifests, alloc.clone())?;
    }
    if !kept.is_empty() && kept[..] != last_programs[..] {
        save_last(&programs_at, &kept, alloc.clone())?;
    }
    let keep_took = began.elapsed();
    // Placed: every answer that built or certified the program placed its path last.
    let linked = !kept.is_empty() && matches!(outcome, Outcome::Built { .. } | Outcome::Certified { .. });
    let mut places = Vec::new_in(alloc.clone());
    for failure in failures.iter() {
        if let Failure::Task { unit, failed: TaskFailed::TwoItems { first, second }, .. } = *failure {
            for at in [first, second] {
                places.push((unit, at, front.place(unit, at, alloc.clone())));
            }
        }
    }
    let summary = Summary {
        outcome,
        asks,
        units,
        phases: Phases { sweep: sweep_took, plan: plan_took, look: look_took, gather: gather_took, host: host_took, keep: keep_took },
        sweep: fresh.cost,
        reads: u32::try_from(reads.asks.len()).unwrap_or(u32::MAX),
        gathered: !held,
        store_error,
        failures,
        told: front.told(alloc),
        places,
    };
    Ok(Built { summary, program: linked.then_some(path), notes: images.notes() })
}

/// The one program `build` and `run` link: [`NO_BINARY`] when there is none, [`TWO_BINARIES`]
/// (each named) when there are more.
fn one_program<A: Allocator + Clone>(graph: &Graph<A>) -> Result<&Program<A>, CommandError> {
    match &graph.programs[..] {
        [one] => Ok(one),
        [] => Err(CommandError::Program { rule: NO_BINARY, what: graph.checkout.to_string().into_boxed_str() }),
        many => {
            // The error's lifetime: the global heap, as every `CommandError` text is.
            let mut names = blaze_mem::String::new_in(Global);
            for (at, program) in many.iter().enumerate() {
                let comma = if at == 0 { "" } else { ", " };
                blaze_mem::text::format_into(&mut names, format_args!("{comma}`{}`", program.name));
            }
            Err(CommandError::Program { rule: TWO_BINARIES, what: names.into_boxed_str() })
        }
    }
}

/// What the command links `program` with, and its file's path: the toolchain for the graph's
/// target (its unit's rustflags, run here), the policy of a file build at the program's
/// profile, the linker's output for the target, and `<out_dir>/<name>`.
fn linking_of<A: Allocator + Clone + Send + Sync>(
    graph: &Graph<A>,
    front: &crate::front::Command<'_, A>,
    program: &Program<A>,
    out_dir: &Path,
    alloc: A,
) -> Result<(Linking<A>, PathBuf), CommandError> {
    let refuse = |rule: &'static str, what: String| CommandError::Program { rule, what: what.into_boxed_str() };
    let target_data = front.workspace.target.as_ref().map_err(|error| refuse(NO_PROGRAM_TOOLCHAIN, format!("{}: {error}", graph.triple)))?;
    let toolchain = hir_ty_codegen::v2::toolchain(&graph.triple, target_data, &program.rustflags, hir_ty_codegen::v2::Runs::Here)
        .map_err(|error| refuse(NO_PROGRAM_TOOLCHAIN, format!("`{}` for {}: {error}", program.name, graph.triple)))?;
    let policy = Policy {
        toolchain,
        profile: program.profile.clone(),
        mode: CodeMode::Output,
        // A dev program's code is stencils or Cranelift's: folding costs no code speed there.
        fold: true,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
        proc_macro_bridge: false,
        // Not known before the walk (whether the program sets a global allocator): off.
        heap_to_stack: false,
        report: false,
        fuel: false,
    };
    let output = blaze_link::target::output(&graph.triple, None, blaze_link::Debug::None, true, blaze_link::target::RunsOn::Here)
        .map_err(|error| refuse(NO_LINK_OUTPUT, format!("{}: {error}", graph.triple)))?;
    let to = blaze_core::FileName::new(&program.name).ok_or_else(|| refuse(NOT_A_FILE_NAME, format!("`{}`", program.name)))?;
    let path = out_dir.join(&*program.name);
    if path.to_str().is_none() {
        return Err(refuse(PATH_NOT_UTF8, path.display().to_string()));
    }
    let linking = Linking {
        unit: program.unit,
        policy,
        settings: crate::code::FrontSettings { image: false, build_time: false },
        name: crate::boxed(&program.name, alloc),
        to,
        output,
    };
    Ok((linking, path))
}

/// The walker's settings of `policy`: the target and CPU, the code generator's features, the
/// toolchain's id, the edit tier (the release tier is refused by the code key,
/// [`program::NO_RELEASE_TIER`]) and the profile's checks.
fn walk_settings(policy: &Policy) -> WalkSettings {
    let toolchain = &policy.toolchain;
    let target = Hasher::new("blaze-command walk target 1").part(toolchain.target.as_bytes()).part(toolchain.cpu.as_bytes()).finish();
    let mut features = Hasher::new("blaze-command walk features 1");
    for feature in &toolchain.features {
        features.part(feature.as_bytes());
    }
    WalkSettings {
        target,
        features: features.finish(),
        toolchain: toolchain.id,
        tier: Tier::Edit,
        panic: match policy.profile.panic {
            Panic::Unwind => WalkPanic::Unwind,
            Panic::Abort => WalkPanic::Abort,
        },
        debuginfo: match policy.profile.debuginfo {
            DebugInfo::None => WalkDebugInfo::None,
            DebugInfo::LineTablesOnly => WalkDebugInfo::Lines,
            DebugInfo::Limited | DebugInfo::Full => WalkDebugInfo::Full,
        },
        overflow_checks: policy.profile.overflow_checks,
    }
}

/// One table of a program's closure, its records copied 16-aligned out of the store.
struct ClosureTable {
    unit: u32,
    manifest: Key<UnitManifest>,
    manifest_bytes: Aligned,
    surface: Aligned,
}

impl ClosureTable {
    /// Its [`Table`]: the surface, and the facts the unit's bodies made (its manifest's `made`).
    fn table<A: Allocator + Clone>(&self, graph: &Graph<A>) -> Result<Table<'_>, CommandError> {
        let unit = &graph.units[self.unit as usize];
        let broke = |why: String| CommandError::Program { rule: NO_CLOSURE_SURFACE, what: format!("{}: {why}", named(unit)).into_boxed_str() };
        let root = record::open::<UnitManifest>(&self.manifest_bytes[..], &self.manifest).map_err(|error| broke(error.to_string()))?;
        let made = root.made.follow(&self.manifest_bytes[..]).map_err(|error| broke(error.to_string()))?;
        Table::of(self.unit, unit.owner, &self.surface[..], made).map_err(|why| broke(format!("{why:?}")))
    }
}

/// The tables of `program`'s closure (it and every unit it depends on, to the end), sorted by
/// owner as [`Universe::tables`] is, each read from the manifest the units' request filed.
/// One manifest and one surface read per unit of the closure.
fn closure_of<A: Allocator + Clone, S: Store, B: Allocator + Clone>(
    graph: &Graph<A>,
    program: u32,
    manifests: &[Key<UnitManifest>],
    store: &S,
    alloc: B,
) -> Result<Vec<ClosureTable, B>, CommandError> {
    let mut seen = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    seen.resize(graph.units.len(), false);
    let mut stack = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    stack.push(program);
    let mut out = Vec::with_capacity_in(graph.units.len(), alloc);
    while let Some(at) = stack.pop() {
        if std::mem::replace(&mut seen[at as usize], true) {
            continue;
        }
        let unit = &graph.units[at as usize];
        stack.extend(unit.deps.iter().map(|dep| dep.unit));
        let broke = |why: &str| CommandError::Program { rule: NO_CLOSURE_SURFACE, what: format!("{}: {why}", named(unit)).into_boxed_str() };
        let manifest = manifests.get(at as usize).copied().ok_or_else(|| broke("no manifest was filed for it"))?;
        let manifest_bytes = read_aligned(store, &manifest)?.ok_or_else(|| broke("the store holds no record under its manifest's key"))?;
        let root = record::open::<UnitManifest>(&manifest_bytes[..], &manifest).map_err(|error| broke(&error.to_string()))?;
        let surface_key: Key<Surface> = Key::from_id(root.surface);
        let surface = read_aligned(store, &surface_key)?.ok_or_else(|| broke("the store holds no record under its surface's key"))?;
        out.push(ClosureTable { unit: at, manifest, manifest_bytes, surface });
    }
    out.sort_by_key(|table| graph.units[table.unit as usize].owner);
    Ok(out)
}

/// The record of kind `K` at `key`, copied 16-aligned; `None` when the store holds none.
fn read_aligned<K: Kind, S: Store>(store: &S, key: &Key<K>) -> Result<Option<Aligned>, CommandError> {
    let read = store.read::<K, Global>(key, Global).map_err(|error| CommandError::Store { what: error.to_string().into_boxed_str() })?;
    Ok(read.map(|read| Aligned::copy_from(&read.bytes[..])))
}

/// The program's walker, its roots listed when its walk begins: over the surfaces of the
/// program's closure as the one request settled them ([`Walker::begin`]'s `units`), so a build
/// keys each unit once (rule 77). Listing them before the request took a request of the units
/// alone first, which keyed every unit a second time.
///
/// The listing reads one manifest and one surface per unit of the closure and runs the front
/// end's roots pass on the driver's thread, between the units' phase and the walk, where nothing
/// else of the request can run. A refusal is kept in `failed` as the command's own error (what
/// [`closure_of`] and [`roots_failed`] name) and fails the program; [`build`] returns it.
struct Rooted<'r, 'c, A: Allocator + Clone + Send + Sync, S: Store> {
    inner: ProgramWalker<program::Reader<'r>>,
    graph: &'r Graph<A>,
    front: &'r crate::front::Command<'c, A>,
    program: &'r Program<A>,
    policy: &'r Policy,
    settings: crate::code::FrontSettings,
    store: &'r S,
    /// Whether the roots are given: a certified answer gives none and walks nothing.
    listed: bool,
    failed: &'r mut Option<CommandError>,
}

/// What a refused listing fails the program with; the error itself is in [`Rooted::failed`].
const ROOTS_REFUSED: TaskFailed = TaskFailed::Refused { what: program::NO_ROOTS };

impl<A: Allocator + Clone + Send + Sync, S: Store> Rooted<'_, '_, A, S> {
    /// The program's roots over `units`, given to the inner walker.
    fn list(&mut self, units: &[Key<UnitManifest>]) -> Result<(), CommandError> {
        let unit = self.program.unit;
        let parts = closure_of(self.graph, unit, units, self.store, Global)?;
        let mut tables = Vec::with_capacity_in(parts.len(), Global);
        for part in parts.iter() {
            tables.push(part.table(self.graph)?);
        }
        let universe = Universe { asker: unit, tables: &tables, outside: self.front.late(unit) };
        let _span = tracing::info_span!(target: "blaze", "command: the program's roots", unit).entered();
        let listed = program::roots(self.front, unit, &universe, self.policy, self.settings)
            .map_err(|failed| roots_failed(self.graph, self.front, self.program, failed))?;
        // The walker copies the asks: the listed roots go with this call.
        self.inner.add_program(&listed.roots(Global));
        Ok(())
    }
}

impl<A: Allocator + Clone + Send + Sync, S: Store> Walker for Rooted<'_, '_, A, S> {
    fn begin(&mut self, program: u32, kept: Kept<'_>, moved: &[Key<PlanRecord>], units: &[Key<UnitManifest>]) -> Result<(), TaskFailed> {
        if !self.listed {
            self.listed = true;
            if let Err(error) = self.list(units) {
                *self.failed = Some(error);
                return Err(ROOTS_REFUSED);
            }
        }
        self.inner.begin(program, kept, moved, units)
    }

    fn ask(&mut self, program: u32) -> Option<WalkAsk<'_>> {
        self.inner.ask(program)
    }

    fn planned(&mut self, program: u32, ask: u32, plan: Planned<'_>) -> Result<(), TaskFailed> {
        self.inner.planned(program, ask, plan)
    }

    fn coded(&mut self, program: u32, ask: u32, code: Key<CodeRecord>) -> Result<(), TaskFailed> {
        self.inner.coded(program, ask, code)
    }

    fn end(&mut self, program: u32) -> Result<WalkEnd<'_>, TaskFailed> {
        self.inner.end(program)
    }
}

/// `program::roots`'s refusal as the command's error: its rule, and what the front end told
/// under the program's unit.
fn roots_failed<A: Allocator + Clone + Send + Sync>(
    graph: &Graph<A>,
    front: &crate::front::Command<'_, A>,
    program: &Program<A>,
    failed: TaskFailed,
) -> CommandError {
    let rule = match failed {
        TaskFailed::Refused { what } | TaskFailed::Panicked { what } | TaskFailed::RefusedFact { what, .. } => what,
        TaskFailed::TwoItems { .. } | TaskFailed::NoBody { .. } => program::NO_ROOTS,
    };
    // The error's lifetime: the global heap, as every `CommandError` text is.
    let mut what = blaze_mem::String::new_in(Global);
    blaze_mem::text::format_into(&mut what, format_args!("{} (`{}`)", named(&graph.units[program.unit as usize]), program.name));
    for (unit, line) in front.told(Global).iter() {
        if *unit == program.unit {
            blaze_mem::text::format_into(&mut what, format_args!("\n  {line}"));
        }
    }
    CommandError::Program { rule, what: what.into_boxed_str() }
}

/// What one request answered, as [`Requested`] holds it: each unit's tasks and records, the
/// manifests by crate key, the failures. The caller sets whether the look held and the times.
fn requested_of<F: Front, A: Allocator + Clone>(graph: &Graph<A>, counting: &Counting<'_, F>, answered: Answered<'_>, alloc: A) -> Requested<A> {
    let mut units = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for at in 0..graph.units.len() {
        units.push(unit_done(&counting.tally[at], answered.units.get(at)));
    }
    let mut manifests = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    for (unit, manifest) in graph.units.iter().zip(answered.manifests.iter()) {
        if let Some(manifest) = manifest {
            manifests.push((unit.key, manifest.id()));
        }
    }
    Requested {
        outcome: answered.outcome,
        asks: answered.asks,
        units,
        manifests,
        programs: Vec::new_in(alloc),
        failures: answered.failures,
        store_error: answered.store_error,
        looked: !answered.looked.is_empty(),
        held: false,
        look_took: Duration::ZERO,
        gather_took: Duration::ZERO,
        host_took: Duration::ZERO,
    }
}

/// One unit's [`UnitDone`]: its tally, and what it holds at the end.
fn unit_done(tally: &Tally, proved: Option<&UnitProved<'_>>) -> UnitDone {
    let mut done = UnitDone {
        keyed: tally.keyed.load(Ordering::Relaxed),
        surfaces: tally.surfaces.load(Ordering::Relaxed),
        bodies: tally.bodies.load(Ordering::Relaxed),
        failed: tally.failed.load(Ordering::Relaxed),
        ..UnitDone::default()
    };
    if let Some(proved) = proved {
        done.manifest = proved.manifest.is_some();
        for (_, holds) in &proved.records {
            match holds.tag() {
                tag if tag == Surface::TAG => done.surface += 1,
                tag if tag == Body::TAG => done.body += 1,
                _ => {}
            }
        }
    }
    done
}

/// `run`'s last step: this process becomes `program` with `args` (exec: the program's exit is
/// the command's, its signals its own). Returns only when the exec failed, with the error
/// naming the program ([`NOT_STARTED`]).
pub fn start(program: &Path, args: &[std::ffi::OsString]) -> CommandError {
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(program).args(args).exec();
    CommandError::Program { rule: NOT_STARTED, what: format!("{}: {error}", program.display()).into_boxed_str() }
}

/// What the one snapshot of [`check`] hands out: nothing branded with it.
struct Requested<A: Allocator> {
    outcome: Outcome,
    asks: Asks,
    units: Vec<UnitDone, A>,
    manifests: Vec<(ContentId, ContentId), A>,
    /// Each program's kept manifest, by its identity ([`ProgramPlan::program`]): empty for
    /// `check`.
    programs: Vec<(ContentId, ContentId), A>,
    failures: Vec<Failure, Global>,
    store_error: Option<StoreError>,
    /// Whether the host hashed any file (the leaf memory moved).
    looked: bool,
    /// Whether the certificate's look held.
    held: bool,
    look_took: Duration,
    gather_took: Duration,
    host_took: Duration,
}

/// The certificate's look, which the command makes before it gathers any text: the request's
/// own lookup ([`Lookup`], the driver's first step), run ahead of it over the same store and
/// the same answers (the leaf memory the sweep just refreshed, [`Answerer`]). The lookup, done,
/// which the request takes as its own ([`blaze_host::run`]'s `certificate_lookup`): a hit or a
/// miss. Reads at most [`blaze_keyed::outside::MAX_SHAPES`] shape slots and as many values,
/// then is done: bounded.
///
/// Its cost is the request's no-op itself: the shape slot read, one answer per read (a stat
/// where the leaf memory holds the file), the value read; each record copied once only where
/// the store's buffer is not 16-aligned.
fn look_up<'s, S: Store, W: Files>(
    snap: Snapshot<'s>,
    certificate: Key<Certificate>,
    store: &S,
    files: &W,
    leaves: Leaves<'_>,
) -> Result<CertificateLookup<'s>, CommandError> {
    let _span = tracing::info_span!(target: "blaze", "command: the certificate's look, before any text is gathered").entered();
    let mut lookup = Lookup::new(snap, certificate);
    let mut checks = Checks::default();
    let mut answerer = Answerer::new(files, leaves);
    let mut answers = Vec::new_in(Global);
    loop {
        match lookup.want() {
            Want::ReadShape(at) => with_record(store, at, |record| {
                if let Some(shape) = lookup.shape(record, &mut checks).map_err(failed)? {
                    answerer.answer_shape(shape, &mut answers).map_err(failed)?;
                    lookup.answers(&answers, &mut checks).map_err(failed)?;
                }
                Ok::<(), CommandError>(())
            })??,
            Want::ReadValue(at) => with_record(store, at, |record| lookup.value(record, &mut checks).map_err(failed))??,
            Want::Done => {
                return match lookup.found() {
                    Some(_) => Ok(lookup),
                    None => Err(failed("the lookup was done and found nothing")),
                };
            }
            Want::Answers | Want::PutShape(_) | Want::PutValue(_) => {
                return Err(failed("the lookup asked for answers or a put, which only a filing asks"));
            }
        }
    }
}

/// Why the certificate's look stopped: `what`.
fn failed(what: impl fmt::Display) -> CommandError {
    CommandError::Certificate { what: what.to_string().into_boxed_str() }
}

/// `then` of the certificate record at `key` (none when the store holds none), 16-aligned:
/// the store's buffer when it is, else one copy.
fn with_record<S: Store, R>(store: &S, key: Key<Certificate>, then: impl FnOnce(Option<&[u8]>) -> R) -> Result<R, CommandError> {
    let read = store.read::<Certificate, Global>(&key, Global).map_err(|error| CommandError::Certificate {
        what: format!("the store's read of a certificate record: {error}").into_boxed_str(),
    })?;
    let Some(read) = read else {
        return Ok(then(None));
    };
    let bytes = &read.bytes[..];
    if bytes.as_ptr().addr() % blaze_format::ALIGN == 0 {
        return Ok(then(Some(bytes)));
    }
    let copied = Aligned::copy_from(bytes);
    Ok(then(Some(&copied[..])))
}

/// How a unit is named to a person: its crate and its package.
pub fn named<A: Allocator + Clone>(unit: &Unit<A>) -> Named<'_, A> {
    Named(unit)
}

/// A unit by name, written as `` `crate` (package version) ``.
pub struct Named<'u, A: Allocator + Clone>(&'u Unit<A>);

impl<A: Allocator + Clone> fmt::Display for Named<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` ({})", self.0.name, self.0.package)
    }
}

/// The checkout's id among kept files: of its directory, which is all that tells two
/// checkouts' leaf tables apart.
fn place_of(checkout: &str) -> PathBuf {
    let id = Hasher::new("blaze-command checkout 1").part(checkout.as_bytes()).finish();
    let mut hex = String::with_capacity(32);
    for byte in &id.0[..16] {
        hex.push_str(&format!("{byte:02x}"));
    }
    PathBuf::from(hex)
}

/// The settings of the keyed store under `keyed`: its `store.json`, whose form must be the
/// plain store's when it names one (the keyed driver files into no other).
pub fn store_settings(keyed: &Path) -> Result<StoreSettings, CommandError> {
    let dir = keyed.join("store");
    let settings = StoreSettings::read(&dir).map_err(|what| CommandError::Store { what: what.into_boxed_str() })?;
    let named = dir.join(blaze_cas::plain::settings::FILE).exists();
    if named && settings.form != Form::Plain {
        return Err(CommandError::Store {
            what: format!("{} names the packs store, and the keyed driver files into the plain store alone", dir.join("store.json").display())
                .into_boxed_str(),
        });
    }
    Ok(StoreSettings { form: Form::Plain, ..settings })
}

fn open_store(keyed: &Path) -> Result<PlainStore, CommandError> {
    let settings = store_settings(keyed)?;
    PlainStore::open(&keyed.join("store"), &settings).map_err(|error| CommandError::Store { what: error.to_string().into_boxed_str() })
}

// --- the disk the front end gathers from ------------------------------------------------------------

/// The bytes one `read` call takes at a time into the caller's buffer.
const READ_CHUNK: usize = 64 << 10;

/// A file of the checkout that is one of the request's reads moved between the sweep and the
/// gathering: the key would be of other bytes than the request's answer.
pub const MOVED_SINCE_SWEEP: &str =
    "gathering a unit: a file of the checkout moved between the sweep and its read (rule: a key is of the bytes the request's read answers); run again";
/// The disk refused a read or a stat.
pub const DISK_REFUSED: &str = "gathering a unit: the disk refused a read or a stat of a file the unit names";

/// The disk, as [`Gathers::gather`] reads it: each file read once into the front end's buffer,
/// in chunks of the caller's fixed buffer. A file of the checkout that is one of the request's
/// reads must hash to the answer the request hands the host for it ([`MOVED_SINCE_SWEEP`]), so
/// what is keyed is what is certified; a file outside the checkout (the standard library, a
/// fetched source) is covered by the tool's library digest and the graph's identity.
struct DiskSource<'r> {
    checkout: &'r str,
    asks: &'r [Ask<'r>],
    then: &'r [ContentId],
    buffer: [u8; READ_CHUNK],
}

impl DiskSource<'_> {
    /// The answer the request holds for `path`, when it is one of its reads.
    fn answer(&self, path: &str) -> Option<ContentId> {
        let relative = path.strip_prefix(self.checkout)?.strip_prefix('/')?;
        // Every ask of [`Reads`] is a file's, in path order.
        let at = self
            .asks
            .binary_search_by(|ask| match ask {
                Ask::File(read) | Ask::Var(read) => (*read).cmp(relative.as_bytes()),
            })
            .ok()?;
        self.then.get(at).copied()
    }
}

impl Source for DiskSource<'_> {
    fn read<B: Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, SourceError> {
        let _span = tracing::debug_span!(target: "blaze", "command: reading a unit's file", path).entered();
        into.clear();
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(DISK_REFUSED),
        };
        loop {
            let read = file.read(&mut self.buffer).map_err(|_| DISK_REFUSED)?;
            if read == 0 {
                break;
            }
            into.extend_from_slice(&self.buffer[..read]);
        }
        match self.answer(path) {
            Some(answer) if answer != file_answer(&into[..]) => Err(MOVED_SINCE_SWEEP),
            _ => Ok(true),
        }
    }

    fn exists(&mut self, path: &str) -> Result<bool, SourceError> {
        let _span = tracing::debug_span!(target: "blaze", "command: asking whether a unit's file is there", path).entered();
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(DISK_REFUSED),
        }
    }
}

// --- the manifests kept between commands ----------------------------------------------------------

const LAST_MAGIC: [u8; 4] = *b"BZLM";
/// The layout word of the kept manifests; a file of any other is refused by it.
const LAST_LAYOUT: u32 = 1;

/// The manifests the last command ended with, by crate key, sorted: none there is the first
/// run's; a file that does not decode is refused by name.
///
/// The layout, little-endian: `magic "BZLM" | layout u32 | count u32 | count x (crate key 32 B,
/// manifest key 32 B) | crc32c u32` of every byte before it.
fn load_last<A: Allocator + Clone>(path: &Path, alloc: A) -> Result<Vec<(ContentId, ContentId), A>, CommandError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new_in(alloc)),
        Err(error) => return Err(CommandError::File { path: path.display().to_string().into_boxed_str(), what: error.to_string().into_boxed_str() }),
    };
    decode_last(&bytes, alloc).map_err(|why| CommandError::Memory {
        path: path.display().to_string().into_boxed_str(),
        what: format!("not the kept manifests: {why}; remove it and the next command settles each unit by its mark").into_boxed_str(),
    })
}

pub(crate) fn decode_last<A: Allocator>(bytes: &[u8], alloc: A) -> Result<Vec<(ContentId, ContentId), A>, &'static str> {
    if bytes.len() < 16 {
        return Err("shorter than a head and a check");
    }
    let body = bytes.len() - 4;
    if bytes[..4] != LAST_MAGIC {
        return Err("its magic is not BZLM");
    }
    let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if word(4) != LAST_LAYOUT {
        return Err("another layout word than this blaze-command's");
    }
    if word(body) != crc32c(&bytes[..body]) {
        return Err("its check is not its bytes': torn or rotten");
    }
    let count = word(8) as usize;
    if 12 + count.saturating_mul(64) != body {
        return Err("its count is not its length");
    }
    let mut rows = Vec::with_capacity_in(count, alloc);
    for at in (12..body).step_by(64) {
        let id = |from: usize| ContentId(bytes[from..from + 32].try_into().expect("32 bytes"));
        rows.push((id(at), id(at + 32)));
    }
    if rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err("its rows are not sorted by crate key, each once");
    }
    Ok(rows)
}

pub(crate) fn encode_last<A: Allocator>(rows: &[(ContentId, ContentId)], alloc: A) -> Vec<u8, A> {
    let mut out = Vec::with_capacity_in(16 + 64 * rows.len(), alloc);
    out.extend_from_slice(&LAST_MAGIC);
    out.extend_from_slice(&LAST_LAYOUT.to_le_bytes());
    out.extend_from_slice(&u32::try_from(rows.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (key, manifest) in rows {
        out.extend_from_slice(&key.0);
        out.extend_from_slice(&manifest.0);
    }
    let check = crc32c(&out);
    out.extend_from_slice(&check.to_le_bytes());
    out
}

fn save_last<A: Allocator>(path: &Path, rows: &[(ContentId, ContentId)], alloc: A) -> Result<(), CommandError> {
    leaves::write_whole(path, &encode_last(rows, alloc))
}

// --- the counting front ---------------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    keyed: AtomicU32,
    surfaces: AtomicU32,
    bodies: AtomicU32,
    failed: AtomicU32,
}

/// The front end handed to the host, each task it runs counted for its unit: the per-unit
/// numbers [`Summary::units`] prints, which the driver's [`blaze_keyed::driver::Ran`] gives only
/// for the whole request. One relaxed add per task.
pub struct Counting<'f, F: Front> {
    front: &'f F,
    // The request's lifetime, shared by the pool's workers: the global heap, which is `Sync`
    // whatever allocator the command was handed.
    tally: Vec<Tally, Global>,
}

impl<'f, F: Front> Counting<'f, F> {
    fn new(front: &'f F, units: usize) -> Counting<'f, F> {
        let mut tally = Vec::with_capacity_in(units, Global);
        tally.resize_with(units, Tally::default);
        Counting { front, tally }
    }

    fn count<T>(&self, unit: u32, which: fn(&Tally) -> &AtomicU32, done: Result<T, TaskFailed>) -> Result<T, TaskFailed> {
        if let Some(tally) = self.tally.get(unit as usize) {
            which(tally).fetch_add(1, Ordering::Relaxed);
            if done.is_err() {
                tally.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
        done
    }
}

impl<F: Front> Front for Counting<'_, F> {
    fn key<B: Allocator + Clone>(&self, unit: u32, frame: B) -> Result<Keyed<B>, TaskFailed> {
        self.count(unit, |it| &it.keyed, self.front.key(unit, frame))
    }

    fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.count(unit, |it| &it.surfaces, self.front.surface(unit, key, universe))
    }

    fn body(&self, unit: u32, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.count(unit, |it| &it.bodies, self.front.body(unit, item, key, universe))
    }

    // The host calls this, never `body`: counted here, handed on whole, so the inner front's
    // own `body_of` (blaze-front's MIR-alone bodies) runs, not the trait's refusing default.
    fn body_of(&self, unit: u32, item: StableId, key: Key<Body>, steps: Steps, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.count(unit, |it| &it.bodies, self.front.body_of(unit, item, key, steps, universe))
    }

    // A program's tasks are of no unit: handed on whole and counted by the driver's `Ran`
    // alone, so the inner front's own (`program.rs`'s) run, not the trait's refusing defaults.
    fn plan(&self, program: u32, ask: &[u8], key: Key<PlanRecord>, bodies: &[GivenBody<'_>], universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.front.plan(program, ask, key, bodies, universe)
    }

    fn code(
        &self,
        program: u32,
        key: Key<CodeRecord>,
        plan: &[u8],
        callees: &[(Key<PlanRecord>, &[u8])],
        bodies: &[GivenBody<'_>],
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        self.front.code(program, key, plan, callees, bodies, universe)
    }

    fn link<B: Allocator + Clone>(
        &self,
        program: u32,
        changes: &[RowChange],
        code: &[&[u8]],
        plans: &[(Key<PlanRecord>, &[u8])],
        spans: &[(ContentId, &[u8])],
        payload: &[u8],
        frame: B,
    ) -> Result<Linked<B>, TaskFailed> {
        self.front.link(program, changes, code, plans, spans, payload, frame)
    }

    // A span task is a program's too (its owner's unit names the snapshot, not a count):
    // handed on whole, so the inner front's own (`program::spans`) runs, not the refusing default.
    fn spans<B: Allocator + Clone>(&self, unit: u32, owner: StableId, universe: Universe<'_>, frame: B) -> Result<SpanTables<B>, TaskFailed> {
        self.front.spans(unit, owner, universe, frame)
    }
}

// --- the panic hook -----------------------------------------------------------------------------

/// `check` runs in flight, each holding a [`Quiet`].
static QUIET: AtomicU32 = AtomicU32::new(0);

/// While held, the command's panic hook prints nothing for a typed refusal
/// ([`crate::front::is_refusal`]: hir-def's `Unreadable`, a keyed file's included, and hir-ty's
/// `Unserved`), which the task's [`crate::front::Command`] turns into its failure and tells
/// under its unit. Dropped, the hook is as it was: it hands every panic to the hook before it.
pub(crate) struct Quiet(());

impl Drop for Quiet {
    fn drop(&mut self) {
        QUIET.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The command's panic hook, set once a process at the first `check` ([`check`], around the
/// host's run): a typed refusal raised while a [`Quiet`] is held prints nothing; every other
/// panic, and every panic while none is held, goes to the hook that was there before (std's,
/// or blaze-profile's), as today. It is never unset (a hook set by another thread after it
/// would be lost by a restore); held by a count, it is restored in effect when the last
/// [`Quiet`] drops.
pub(crate) fn quiet_refusals() -> Quiet {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let before = std::panic::take_hook();
        // The process's one wrapper of the hook before it, on the global heap as the process's.
        std::panic::set_hook(Box::new_in(
            move |info: &std::panic::PanicHookInfo<'_>| {
                if QUIET.load(Ordering::Acquire) > 0 && crate::front::is_refusal(info.payload()) {
                    return;
                }
                before(info)
            },
            Global,
        ));
    });
    QUIET.fetch_add(1, Ordering::AcqRel);
    Quiet(())
}

// --- planted bugs, for the tests' controls ---------------------------------------------------------

/// A bug planted in the command for a test's control: with it the command answers what it must
/// not, and the test's oracle says so by name. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The certificate's base takes the sweep's instant: no two commands share a certificate.
    BaseOfRun,
    /// The request leaves its last read out: an edit of that file is never seen.
    SkipsARead,
    /// A unit the command cannot build yet does not stop it.
    IgnoresBlocked,
    /// The command makes no certificate look of its own: a no-op gathers every unit's text.
    NoLook,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = (plant, Plant::None, Plant::BaseOfRun, Plant::SkipsARead, Plant::IgnoresBlocked, Plant::NoLook);
    false
}
