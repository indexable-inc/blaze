//! `blaze prove` (docs/v2/plan/design/proofs-for-sans-io.md, section 5), in its two parts.
//!
//! **On a worker** ([`prove`], behind [`Front::prove`](crate::serve::work::Front::prove)),
//! building nothing more than the view's proc macros and build scripts' runs, which the job
//! builds and applies to its front end first, as a reach does (`Jobs::next`): each function
//! of the view's primary library crates marked `#[blaze::theorem]` translated to Lean
//! (`hir_ty_lean::emit`), each crate's proof files read from the view (`lean/` beside its
//! `Cargo.toml`), and the plan of their check made (`hir_ty_lean::plan`): the files to write
//! and the runs of Lean to make, each keyed by what its verdict depends on. With `--subset`,
//! the translator's outcome for every function in a `#![blaze::sans_io]` module of those
//! crates (translated, generic, or refused for what), by path, and their counts. The worker writes no file and starts no process: what it found is a [`Proving`].
//!
//! **On the loop** ([`Checking`], a [`Runs`]): the plan's files written under the build
//! directory's `lean/` and each step run as an [`Effect::Spawn`] of the pinned Lean in
//! blaze's sandbox, as a build script is. A proof file's tactics can do IO, so Lean reads its
//! own release and that directory, writes that directory alone, and has no network
//! (`sandbox-everything`); a step that has not ended by [`DEADLINE`] is killed and its
//! theorems are open, naming it (`bounded-progress`). Lean never reads the checkout: the
//! proof files are written from the texts the plan keyed. Every theorem's model is compiled,
//! proof file or none ([`plan::wanted`]): a model Lean refuses is answered as blaze's bug
//! ([`ProofRecord::ModelBroken`]), never `Open`. A step Lean accepted leaves its
//! output beside its module under the step's key (its verdict), and a step whose key is found
//! there is not run again: an unchanged tree runs Lean not at all. A verdict is written only
//! after the compiled module it vouches for is ordered before it on the disk
//! ([`Effect::Barrier`]), and carries a hash of itself, so a crash leaves no verdict whose
//! module is not whole and no torn verdict is taken for one. `--emit` writes the files and
//! runs nothing.
//!
//! The crates are the primary units' libraries as `blaze check` selects them, checked without
//! `cfg(test)`: what is translated is the code of the build being proved. Their functions are
//! every function item the crate's item table has ([`blaze_items::crate_hashes`], bodies'
//! items included), in its key order, then by path; the records are sorted by function, so
//! the same tree gives the same bytes whatever order the steps ended in.
//!
//! Cost: one pass over each crate's item keys, the attribute flags of each function (a kept
//! column) and the translation of each theorem (and, with `--subset`, of each function of a
//! core); then per step one small read, and for a step whose key moved one run of Lean.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_api::{ApiError, ProofRecord, Proven, Response, Subset, SubsetFunction, SubsetOutcome, SubsetRefusal};
use blaze_cas::Store;
use blaze_core::{Command, Completion, Done, Effect, FileName, Sandbox, Tag};
use blaze_ids::ContentId;
use blaze_items::{ItemId, Resolved};
use blaze_lower::Db;
use blaze_units::schema::Mode;
use hir_def::{AttrDefId, FunctionId, attrs::AttrFlags};
use hir_ty_lean::{
    Emitted, LEAN_VERSION, NotTranslated,
    plan::{self, Ask, Plan, Ran, Root, Verdict},
};

use super::{Job, Real, answer_macros_built};
use crate::serve::work::{Asks, Failed, RunPuts, Runs};

/// The variable the client reads Lean's path from, named by every error about it.
const LEAN_VARIABLE: &str = "BLAZE_LEAN_PROVER";

/// How long one run of Lean may take before it is killed and what it was checking is open:
/// the bound of every wait of a check. A proof that needs longer says so in its own text
/// (`set_option maxHeartbeats`) and is split; none of blaze's takes ten seconds.
const DEADLINE: Duration = Duration::from_secs(600);
/// How often the runs of Lean in flight are held to [`DEADLINE`].
const TICK: Duration = Duration::from_secs(1);
/// How many runs of Lean go at once: the steps of a crate that import nothing of each other.
/// Lean elaborates a file on one thread for the most part.
const AT_ONCE: usize = 8;

/// A crate whose theorems are proved: its front-end crate and where its proof files are.
struct Member {
    krate: base_db::Crate,
    /// The package's name: the directory its proof files are written in for Lean.
    name: String,
    /// `lean/` beside the package's `Cargo.toml`, on the disk: where a person edits a proof.
    proofs: PathBuf,
    /// The same directory in the view, which is where the worker reads it.
    in_view: Option<String>,
}

/// A function of a crate proved, by its path ([`blaze_items::key::path`]).
struct Function {
    path: String,
    id: FunctionId,
    theorem: bool,
    in_core: bool,
    /// Which crate of the members it is of.
    of: usize,
}

/// One crate's theorems, planned.
struct Asked {
    name: String,
    proofs: PathBuf,
    plan: Plan,
    /// What each step of the plan came to; `None` until it has.
    ran: Vec<Option<Ran>>,
    /// The steps some check needs, in order ([`plan::wanted`]).
    wanted: Vec<usize>,
}

/// What `blaze prove`'s worker found: the refusals, the count, and each crate's plan.
pub struct Proving {
    emit: bool,
    sandbox_off: bool,
    /// The build directory's `lean/`: all that Lean reads but its own release, and all it
    /// writes.
    out: PathBuf,
    records: Vec<ProofRecord>,
    subset: Option<Subset>,
    crates: Vec<Asked>,
}

/// `blaze prove` over `job`'s view, read through `db`: everything but Lean.
pub(crate) fn prove(job: &Job, db: &Db, emit: bool, subset: bool) -> Result<Proving, Failed> {
    let refuse = |what: String| Failed::Error(ApiError::new(what, Some("prove".to_owned())));
    let units = job.units();
    let out = Path::new(units.build_dir.as_str()).join("lean");
    let members = members_of(job, db)?;
    let functions = functions_of(db, &members, subset);

    let mut records = Vec::new();
    // Each core's function with its outcome, in the order of `functions` (by path).
    let mut outcomes: Vec<SubsetFunction> = Vec::new();
    let (mut in_cores, mut translated, mut generic) = (0u32, 0u32, 0u32);
    let mut theorems: Vec<Vec<Emitted>> = members.iter().map(|_| Vec::new()).collect();
    for function in &functions {
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let emitted = {
            let _span = tracing::info_span!(
                target: "blaze",
                "prove: translate to Lean",
                function = %function.path,
                theorem = function.theorem,
            )
            .entered();
            hir_ty_lean::emit(db, function.id)
        };
        if function.in_core && subset {
            in_cores += 1;
            let outcome = match &emitted {
                Ok(_) => {
                    translated += 1;
                    SubsetOutcome::Translated
                }
                Err(refused) if is_generic_unasked(refused) => {
                    generic += 1;
                    SubsetOutcome::Generic
                }
                // clone: one refusal's construct, kept once in the answer
                Err(refused) => SubsetOutcome::Refused { what: refused.what.clone() },
            };
            // clone: the answer names each function once
            outcomes.push(SubsetFunction { path: function.path.clone(), outcome });
        }
        if !function.theorem {
            continue;
        }
        match emitted {
            Ok(emitted) if emitted.statement.is_none() => {
                // The translator states every theorem function it translates (`emit`): one
                // translated with no statement is its bug, said as such, never a record
                // skipped.
                return Err(refuse(format!(
                    "`{}` is marked `#[blaze::theorem]`, and hir-ty-lean translated it with no \
                     statement (`<name>.spec.statement`), which it gives every theorem function",
                    emitted.function
                )));
            }
            Ok(emitted) => theorems[function.of].push(emitted),
            Err(refused) => records.push(refused_record(refused)),
        }
    }

    let mut crates = Vec::new();
    for (member, theorems) in members.iter().zip(theorems) {
        if theorems.is_empty() {
            continue;
        }
        // `--emit` checks nothing, so it reads no proof file.
        let proofs = match emit {
            true => BTreeMap::new(),
            false => proof_files(job, member)?,
        };
        let plan = plan::plan(Ask { theorems: &theorems, proofs: &proofs })
            .map_err(|error| refuse(format!("`blaze prove` could not plan `{}`'s check: {error}", member.name)))?;
        let wanted = plan::wanted(&plan);
        crates.push(Asked {
            // clone: the plan's crate owns its name and its place, once per crate
            name: member.name.clone(),
            proofs: member.proofs.clone(),
            ran: plan.steps.iter().map(|_| None).collect(),
            wanted,
            plan,
        });
    }
    let subset = subset.then(|| Subset {
        refused: counted(&outcomes),
        outcomes,
        functions: in_cores,
        translated,
        generic,
    });
    Ok(Proving { emit, sandbox_off: job.sandbox_off(), out, records, subset, crates })
}

/// The primary units' library crates, each once, with their proof files' directories: the
/// crates `blaze check` with the request's flags checks as libraries, without `cfg(test)`.
fn members_of(job: &Job, db: &Db) -> Result<Vec<Member>, Failed> {
    let units = job.units();
    let root = job.input.config.invocation.root.trim_end_matches('/');
    let mut members: Vec<Member> = Vec::new();
    for (at, unit) in units.graph.units.iter().enumerate() {
        if unit.is_std
            || unit.macro_wasm
            || unit.mode != Mode::Check
            || unit.checked_as_test
            || !unit.target.is_lib()
            || !units.is_primary(at)
        {
            continue;
        }
        let Some(krate) = job.crates.krate(at) else { continue };
        if members.iter().any(|it| it.krate == krate) {
            continue;
        }
        // A crate whose proc macros are not built has items its def map does not have yet. The
        // job built and applied the view's proc macros before posting this; checked again here.
        answer_macros_built(db, krate, &unit.target.name, "prove")?;
        let package = job.package(unit)?;
        let manifest = Path::new(package.manifest_path.as_str());
        let Some(dir) = manifest.parent() else {
            return Err(Failed::Error(ApiError::new(
                format!(
                    "the manifest `{}` of package `{}` is in no directory, where its proof files' \
                     `lean/` would be",
                    package.manifest_path, package.name
                ),
                Some("prove".to_owned()),
            )));
        };
        let proofs = dir.join("lean");
        // The view holds the checkout: a package outside it (a path dependency made a member)
        // has no proof files the view can give.
        let in_view = proofs.to_str().and_then(|path| {
            let rest = path.strip_prefix(root)?;
            match rest.strip_prefix('/') {
                Some(rest) => Some(rest.to_owned()),
                None if rest.is_empty() => Some(String::new()),
                None => None,
            }
        });
        // clone: the member owns its name, once per crate
        members.push(Member { krate, name: package.name.clone(), proofs, in_view });
    }
    Ok(members)
}

/// The functions of `members` that are theorems, or with `subset` in a sans-IO core, each
/// once, by path; the same path twice (two impls' methods of one name) keeps the item
/// table's key order.
fn functions_of(db: &Db, members: &[Member], subset: bool) -> Vec<Function> {
    let mut functions = Vec::new();
    for (of, member) in members.iter().enumerate() {
        let table = blaze_items::crate_hashes(db, member.krate);
        let keys = table.keys().iter().copied().chain(table.unhashable().iter().map(|it| it.0));
        for key in keys {
            let Some(Resolved::Item(ItemId::Function(id))) = blaze_items::resolve(db, key) else {
                continue;
            };
            let theorem =
                AttrFlags::query(db, AttrDefId::FunctionId(id)).contains(AttrFlags::THEOREM);
            let in_core = subset && in_core(db, id);
            if !theorem && !in_core {
                continue;
            }
            let path = blaze_items::key::path(db, ItemId::Function(id));
            functions.push(Function { path, id, theorem, in_core, of });
        }
    }
    // Stable: equal paths keep the key order, which is the tree's.
    functions.sort_by(|a, b| a.path.cmp(&b.path));
    functions
}

/// Whether `f` is in a module marked `#![blaze::sans_io]`, or in one inside such a module: the
/// cores the `sans_io_core` lint holds (`blaze_lints::blaze::sans_io_core::core_of`).
fn in_core(db: &Db, f: FunctionId) -> bool {
    let mut module = hir::Function::from(f).module(db);
    loop {
        if AttrFlags::query(db, AttrDefId::ModuleId(module.into())).contains(AttrFlags::SANS_IO) {
            return true;
        }
        match module.parent(db) {
            Some(parent) => module = parent,
            None => return false,
        }
    }
}

/// Every Lean file under the crate's `lean/` as the view has it, by module (`A/B.lean` is
/// `A.B`), with its text: what the plan keys and Lean is given. A crate with no `lean/` has
/// none, and each of its theorems is open for want of a proof file.
fn proof_files(job: &Job, member: &Member) -> Result<BTreeMap<String, String>, Failed> {
    let refuse = |what: String| Failed::Error(ApiError::new(what, Some("prove".to_owned())));
    let mut files = BTreeMap::new();
    let Some(top) = &member.in_view else { return Ok(files) };
    let pinned = job.input.files.pin();
    let root = job.input.root;
    // The directories still to list, each with the module's name so far.
    let mut pending = vec![(top.clone(), String::new())];
    while let Some((dir, module)) = pending.pop() {
        let Some(names) = pinned.list(root, &dir) else { continue };
        for (name, is_dir) in names {
            let path = format!("{dir}/{name}");
            if is_dir {
                pending.push((path, format!("{module}{name}.")));
                continue;
            }
            let Some(stem) = name.strip_suffix(".lean") else { continue };
            let file = pinned
                .file(root, &path)
                .map_err(|error| refuse(format!("the proof file {path} could not be read from the view: {error:?}")))?;
            let Some(file) = file else { continue };
            let text = std::str::from_utf8(file.bytes).map_err(|error| {
                refuse(format!("the proof file {path} is not UTF-8 ({error}): Lean reads text"))
            })?;
            files.insert(format!("{module}{stem}"), text.to_owned());
        }
    }
    Ok(files)
}

fn refused_record(refused: NotTranslated) -> ProofRecord {
    let NotTranslated { function, what, at } = refused;
    ProofRecord::Refused { at, what, function }
}

fn function_of(record: &ProofRecord) -> &str {
    match record {
        ProofRecord::Open { function, .. }
        | ProofRecord::Proved { function, .. }
        | ProofRecord::Emitted { function, .. }
        | ProofRecord::Refused { function, .. }
        | ProofRecord::ModelBroken { function, .. } => function,
    }
}

/// Whether the translator's refusal is its answer for a generic function asked for alone:
/// not a refusal but "translated at each instance a theorem reaches", counted apart. The one
/// place `blaze prove` tells it, by the translator's own text,
/// `hir_ty_lean::body::GENERIC_UNASKED` (crates/hir-ty-lean/src/body.rs), which hir-ty-lean
/// gives whole as the `what` of every such refusal.
fn is_generic_unasked(refused: &NotTranslated) -> bool {
    refused.what == hir_ty_lean::body::GENERIC_UNASKED
}

/// Each refused `what` among `outcomes` with how many functions it refused, most first, then
/// by `what`.
fn counted(outcomes: &[SubsetFunction]) -> Vec<SubsetRefusal> {
    let mut whats: Vec<&str> = outcomes
        .iter()
        .filter_map(|it| match &it.outcome {
            SubsetOutcome::Refused { what } => Some(what.as_str()),
            SubsetOutcome::Translated | SubsetOutcome::Generic => None,
        })
        .collect();
    whats.sort_unstable();
    let mut counted: Vec<SubsetRefusal> = Vec::new();
    for what in whats {
        match counted.last_mut() {
            Some(last) if last.what == what => last.count += 1,
            // clone: one line's construct, once per refusal counted
            _ => counted.push(SubsetRefusal { what: what.to_owned(), count: 1 }),
        }
    }
    counted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.what.cmp(&b.what)));
    counted
}

/// An effect of a check in flight.
enum Flight {
    /// A file of a plan being written.
    Write { path: PathBuf },
    /// `lean --version`.
    Version { since: u64 },
    /// A step's kept verdict being looked for.
    Look { step: usize },
    /// Lean running a step, since the tick `since`; `late` once it was killed for its deadline.
    Lean { step: usize, since: u64, late: bool },
    /// An accepted step's compiled module being ordered before its verdict, which is written
    /// to `kept` once it is.
    Order { kept: PathBuf, key: ContentId, output: String },
    /// A verdict being written, and the tick: neither is waited for by the answer.
    Other,
}

/// A safeguard of the check a test removes, to see that its oracle catches the removal.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A step is started while a module it imports is still with Lean.
    BeforeNeeds,
    /// A step Lean refused leaves a verdict as an accepted one does.
    KeepRefused,
}

/// The check of a [`Proving`] on the loop: see the module.
pub struct Checking {
    proving: Proving,
    /// The `lean` to run; none with `--emit`.
    lean: Option<PathBuf>,
    /// Its release's directory, which the sandbox lets it read.
    release: PathBuf,
    tags: u64,
    next: u64,
    flight: BTreeMap<u64, Flight>,
    /// Ticks since the check began, and whether one is in flight.
    ticks: u64,
    ticking: bool,
    /// The crate being checked: one at a time, so the library's steps are run once and found
    /// by the next.
    at: usize,
    /// Whether the plan's files are written; whether Lean was asked its version, and said it
    /// is the pinned release.
    written: bool,
    asked: bool,
    pinned: bool,
    /// Steps accepted in this check, by key: the library is every crate's.
    accepted: BTreeMap<ContentId, String>,
    /// Why no proof was checked, when none could be.
    failed: Option<String>,
    cancelled: bool,
    #[cfg(test)]
    plant: Plant,
}

impl Checking {
    pub(crate) fn new(proving: Proving, lean: Option<&str>, tags: u64) -> Checking {
        let mut failed = None;
        let (lean, release) = match (proving.emit, lean) {
            (true, _) => (None, PathBuf::new()),
            (false, Some(lean)) => {
                let lean = PathBuf::from(lean);
                // `<release>/bin/lean`: the release holds its own libraries beside `bin`.
                match lean.parent().and_then(Path::parent) {
                    Some(release) => {
                        let release = release.to_owned();
                        (Some(lean), release)
                    }
                    None => {
                        failed = Some(no_lean(format!(
                            "`{}` is not the `lean` of a release (`<release>/bin/lean`)",
                            lean.display()
                        )));
                        (None, PathBuf::new())
                    }
                }
            }
            (false, None) => {
                failed = Some(format!(
                    "`blaze prove` checks proofs with Lean {LEAN_VERSION} and the request names \
                     none: the client reads it from `{LEAN_VARIABLE}` (`--emit` writes the models \
                     and statements with no Lean)"
                ));
                (None, PathBuf::new())
            }
        };
        Checking {
            proving,
            lean,
            release,
            tags,
            next: 0,
            flight: BTreeMap::new(),
            ticks: 0,
            ticking: false,
            at: 0,
            written: false,
            asked: false,
            pinned: false,
            accepted: BTreeMap::new(),
            failed,
            cancelled: false,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    fn tag(&mut self, flight: Flight) -> Tag {
        let tag = self.tags + self.next;
        self.next += 1;
        self.flight.insert(tag, flight);
        Tag(tag)
    }

    /// The directory of `root` for the crate being checked.
    fn dir(&self, root: Root, of: usize) -> PathBuf {
        match root {
            Root::Out => self.proving.out.join("lib"),
            Root::Proofs => self.proving.out.join("proofs").join(&self.proving.crates[of].name),
        }
    }

    fn sandbox(&self) -> Sandbox {
        match self.proving.sandbox_off {
            true => Sandbox::Off,
            false => Sandbox::On {
                // What it may write it may read, and the sandbox refuses a directory named
                // both ways (the read-only entry would cover the writable one): Lean's
                // release is the one thing it only reads.
                // clone: a command owns its paths, once per run of Lean
                read: vec![self.release.clone()],
                write: vec![self.proving.out.clone()],
                network: false,
                trace: false,
                services: Vec::new(),
            },
        }
    }

    /// Lean with `args` in `dir`, searching the two directories of crate `of`.
    fn command(&self, of: usize, dir: PathBuf, args: Vec<OsString>) -> Option<Command> {
        let lean = self.lean.as_ref()?;
        let search = std::env::join_paths([self.dir(Root::Out, of), self.dir(Root::Proofs, of)]).ok()?;
        Some(Command {
            // clone: a command owns its program, once per run of Lean
            program: lean.clone(),
            args,
            env: vec![(OsString::from("LEAN_PATH"), search)],
            dir,
            stdin: Vec::new(),
            sandbox: self.sandbox(),
            memory: None,
        })
    }

    fn tick(&mut self, effects: &mut Vec<Effect>) {
        if !self.ticking {
            self.ticking = true;
            let tag = self.tag(Flight::Other);
            effects.push(Effect::Timer { after: TICK, tag });
        }
    }

    /// Whether a run of Lean is in flight.
    fn running(&self) -> usize {
        self.flight.values().filter(|it| matches!(it, Flight::Lean { .. } | Flight::Version { .. })).count()
    }

    /// The check ends with what it found: always an answer, which says what is proved, what
    /// is open and, where no proof could be checked, why.
    fn answer(&mut self, out: &mut Vec<Response>) -> Option<bool> {
        let mut records = std::mem::take(&mut self.proving.records);
        records.sort_by(|a, b| function_of(a).cmp(function_of(b)).then_with(|| a.cmp(b)));
        out.push(Response::Proven(Proven {
            failed: self.failed.take(),
            subset: self.proving.subset.take(),
            records,
        }));
        Some(true)
    }

    fn fail(&mut self, why: String, out: &mut Vec<Response>) -> Option<bool> {
        self.failed = Some(why);
        self.answer(out)
    }

    /// The verdicts of the crate being checked, as records, once each of its steps has an
    /// outcome; then the next crate.
    fn concluded(&mut self) -> Result<(), String> {
        let asked = &self.proving.crates[self.at];
        // A step no check needs was not run.
        let ran: Vec<Ran> = asked.ran.iter().map(|it| it.clone().unwrap_or(Ran::NotRun)).collect();
        let verdicts = plan::conclude(&asked.plan, &ran).map_err(|error| error.to_string())?;
        let lib = self.proving.out.join("lib");
        for verdict in verdicts {
            self.proving.records.push(match verdict {
                Verdict::Proved { function, theorem, key, axioms, model } => ProofRecord::Proved {
                    key: key.id,
                    model: lib.join(model).display().to_string(),
                    axioms,
                    theorem,
                    function,
                },
                Verdict::Open { function, statement, proof_module, why } => ProofRecord::Open {
                    why: why.to_string(),
                    function,
                    statement,
                    proof_file: asked
                        .proofs
                        .join(format!("{}.lean", proof_module.replace('.', "/")))
                        .display()
                        .to_string(),
                },
                // Lean's place, under the directory the models are written in: a path a
                // person opens.
                Verdict::ModelBroken { function, module, at, error } => ProofRecord::ModelBroken {
                    at: lib.join(at).display().to_string(),
                    error,
                    module,
                    function,
                },
            });
        }
        self.at += 1;
        Ok(())
    }

    /// Goes on with whatever can go on: the steps of the crate being checked whose needs are
    /// accepted, up to [`AT_ONCE`]; the next crate once this one is concluded; the answer once
    /// all are.
    fn go(&mut self, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        if self.cancelled {
            return match self.running() {
                0 => Some(false),
                _ => None,
            };
        }
        if !self.written {
            return None;
        }
        if !self.proving.emit && !self.pinned && !self.proving.crates.is_empty() {
            // Asked once the files are written: the sandbox is of a directory that is there.
            if !self.asked {
                self.asked = true;
                let dir = self.release.clone(); // clone: the command's directory, once
                let Some(command) = self.command(0, dir, vec![OsString::from("--version")]) else {
                    return self.fail("no command could be made to ask Lean its version".to_owned(), out);
                };
                let tag = self.tag(Flight::Version { since: self.ticks });
                blaze_core::reaped::label(tag, "prove: Lean says its version", None);
                effects.push(Effect::Spawn { command: Box::new(command), tag });
                self.tick(effects);
            }
            return None;
        }
        if self.proving.emit {
            let lib = self.proving.out.join("lib");
            for asked in &self.proving.crates {
                for theorem in &asked.plan.theorems {
                    let module = theorem.model.trim_end_matches(".lean").replace('/', ".");
                    self.proving.records.push(ProofRecord::Emitted {
                        path: lib.join(&theorem.model).display().to_string(),
                        module,
                        // clone: the record owns its names
                        function: theorem.function.clone(),
                        statement: theorem.statement.clone(),
                    });
                }
            }
            return self.answer(out);
        }
        loop {
            if self.at == self.proving.crates.len() {
                // A verdict whose module is still being ordered is written before the
                // answer: the next check finds every step this one accepted.
                if self.flight.values().any(|it| matches!(it, Flight::Order { .. })) {
                    return None;
                }
                return self.answer(out);
            }
            let of = self.at;
            let mut waiting = false;
            let wanted = self.proving.crates[of].wanted.clone(); // clone: indices, once a pass
            for step in wanted {
                let asked = &self.proving.crates[of];
                if asked.ran[step].is_some() {
                    continue;
                }
                if self.flight.values().any(|it| {
                    matches!(it, Flight::Lean { step: s, .. } | Flight::Look { step: s } if *s == step)
                }) {
                    waiting = true;
                    continue;
                }
                let needs = &asked.plan.steps[step].needs;
                if needs.iter().any(|need| matches!(asked.ran[*need], Some(Ran::Rejected(_) | Ran::TimedOut | Ran::NotRun))) {
                    self.proving.crates[of].ran[step] = Some(Ran::NotRun);
                    continue;
                }
                #[cfg(test)]
                let before_needs = self.plant == Plant::BeforeNeeds;
                #[cfg(not(test))]
                let before_needs = false;
                if !before_needs && needs.iter().any(|need| asked.ran[*need].is_none()) {
                    waiting = true;
                    continue;
                }
                // Accepted in this check already (the library, for an earlier crate).
                if let Some(output) = self.accepted.get(&asked.plan.steps[step].key) {
                    // clone: Lean's words, once per step found
                    self.proving.crates[of].ran[step] = Some(Ran::Accepted(output.clone()));
                    continue;
                }
                waiting = true;
                if self.running() + self.looks() >= AT_ONCE {
                    continue;
                }
                // Its kept verdict first: a step whose key is found is not run.
                let kept = self.kept(of, step);
                match file_name(&kept) {
                    Ok(file) => {
                        let tag = self.tag(Flight::Look { step });
                        effects.push(Effect::ReadAll { file, tag });
                    }
                    Err(why) => return self.fail(why, out),
                }
            }
            if waiting {
                return None;
            }
            // Every step it wants has an outcome. A pass that only marked steps not run comes
            // here too: nothing is in flight for this crate.
            if let Err(why) = self.concluded() {
                return self.fail(why, out);
            }
        }
    }

    fn looks(&self) -> usize {
        self.flight.values().filter(|it| matches!(it, Flight::Look { .. })).count()
    }

    /// Where the verdict of `step` of crate `of` is kept: beside its module.
    fn kept(&self, of: usize, step: usize) -> PathBuf {
        let it = &self.proving.crates[of].plan.steps[step];
        self.dir(it.root, of).join(format!("{}.verdict", it.module.replace('.', "/")))
    }

    /// The verdict of an accepted step, kept under its key beside its module: written once
    /// the compiled module Lean left is ordered before it, so that no crash keeps the verdict
    /// and loses bytes of the module the next check would import on its word.
    fn keep(&mut self, of: usize, step: usize, key: ContentId, output: String, effects: &mut Vec<Effect>) {
        let it = &self.proving.crates[of].plan.steps[step];
        let kept = self.kept(of, step);
        let Some(olean) = it.olean() else {
            // A check keeps no compiled module: its verdict is what Lean printed.
            self.write_verdict(&kept, &key, &output, effects);
            return;
        };
        if let Ok(file) = file_name(&self.dir(it.root, of).join(olean)) {
            let tag = self.tag(Flight::Order { kept, key, output });
            effects.push(Effect::Barrier { file, tag });
        }
    }

    fn write_verdict(&mut self, kept: &Path, key: &ContentId, output: &str, effects: &mut Vec<Effect>) {
        if let Ok(file) = file_name(kept) {
            let bytes = verdict_file(key, output);
            let tag = self.tag(Flight::Other);
            effects.push(Effect::WriteUnsynced { file, bytes, tag });
        }
    }

    /// Lean started on `step` of the crate being checked.
    fn run(&mut self, step: usize, effects: &mut Vec<Effect>) -> Result<(), String> {
        let of = self.at;
        let it = &self.proving.crates[of].plan.steps[step];
        let dir = self.dir(it.root, of);
        let mut args = vec![OsString::from("--root=.")];
        if let Some(olean) = it.olean() {
            args.push(OsString::from("-o"));
            args.push(dir.join(olean).into_os_string());
        }
        args.push(OsString::from(it.file()));
        let module = it.module.clone(); // clone: a span's and a label's name, once per run of Lean
        let Some(command) = self.command(of, dir, args) else {
            return Err(format!("no command could be made to run Lean on `{module}`"));
        };
        let tag = self.tag(Flight::Lean { step, since: self.ticks, late: false });
        blaze_core::reaped::label(tag, "prove: Lean checks a module", Some(&module));
        effects.push(Effect::Spawn { command: Box::new(command), tag });
        self.tick(effects);
        Ok(())
    }
}

/// `path`, absolute, as the driver names a file: under its root, which is the machine's.
fn file_name(path: &Path) -> Result<FileName, String> {
    path.to_str().and_then(|it| FileName::new(it.trim_start_matches('/'))).ok_or_else(|| {
        format!("{} is no path the daemon's driver can name (it is not UTF-8, or has `..`)", path.display())
    })
}

fn no_lean(why: String) -> String {
    format!(
        "{why}: `{LEAN_VARIABLE}` must name the `lean` of Lean {LEAN_VERSION}, the toolchain \
         `lean/lean-toolchain` pins (rule no-fallback: no proof is checked by another Lean, and \
         none is skipped)"
    )
}

/// The hash a verdict carries of what Lean printed: a file a crash cut short does not have it.
fn verdict_hash(output: &str) -> ContentId {
    ContentId::of("blaze v2 lean verdict 1", output.as_bytes())
}

/// What a kept verdict's file holds for `key`: the key and the hash of what Lean printed,
/// then what it printed.
fn verdict_file(key: &ContentId, output: &str) -> Vec<u8> {
    format!("{key:x} {:x}\n{output}", verdict_hash(output)).into_bytes()
}

/// Lean's output in the kept verdict `bytes`, where it is `key`'s and whole. A file of
/// another key, of another layout or cut short is no verdict, and its step is run.
fn verdict_of(bytes: &[u8], key: &ContentId) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (first, output) = text.split_once('\n')?;
    let (kept_key, hash) = first.split_once(' ')?;
    (kept_key == format!("{key:x}") && hash == format!("{:x}", verdict_hash(output)))
        .then(|| output.to_owned())
}

impl Checking {
    /// The check begins: its files are asked to be written. It may end at once (nothing to
    /// check, or no Lean to check with).
    fn begin(&mut self, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        if self.failed.is_some() {
            return self.answer(out);
        }
        // Every plan's files, each path once: the library's are every crate's.
        let mut writes: BTreeMap<PathBuf, &str> = BTreeMap::new();
        for (of, asked) in self.proving.crates.iter().enumerate() {
            for write in &asked.plan.writes {
                writes.insert(self.dir(write.root, of).join(&write.path), &write.text);
            }
        }
        let writes: Vec<(PathBuf, Vec<u8>)> =
            writes.into_iter().map(|(path, text)| (path, text.as_bytes().to_vec())).collect();
        for (path, bytes) in writes {
            let file = match file_name(&path) {
                Ok(file) => file,
                Err(why) => return self.fail(why, out),
            };
            let tag = self.tag(Flight::Write { path });
            effects.push(Effect::WriteUnsynced { file, bytes, tag });
        }
        self.written = !self.flight.values().any(|it| matches!(it, Flight::Write { .. }));
        self.go(effects, out)
    }

    /// An effect of the check completed.
    fn told(
        &mut self,
        completion: Completion,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let Some(flight) = self.flight.remove(&completion.tag.0) else { return None };
        match (flight, completion.done) {
            (Flight::Write { .. }, Done::Written) => {
                self.written = !self.flight.values().any(|it| matches!(it, Flight::Write { .. }));
            }
            (Flight::Write { path }, done) => {
                return self.fail(
                    format!("`blaze prove` could not write {}: {}", path.display(), said(&done)),
                    out,
                );
            }
            (Flight::Version { .. }, Done::Exited(exit)) => {
                let version = String::from_utf8_lossy(&exit.stdout);
                if exit.code != Some(0) || !version.contains(&format!("version {LEAN_VERSION}")) {
                    let lean = self.lean.as_deref().unwrap_or(Path::new(""));
                    let why = no_lean(format!(
                        "`{} --version` {}, not Lean {LEAN_VERSION}",
                        lean.display(),
                        ended(&exit)
                    ));
                    return self.fail(why, out);
                }
                self.pinned = true;
            }
            (Flight::Version { .. }, done) => {
                let lean = self.lean.as_deref().unwrap_or(Path::new(""));
                let why = no_lean(format!("{} did not start: {}", lean.display(), said(&done)));
                return self.fail(why, out);
            }
            (Flight::Look { step }, done) => {
                let of = self.at;
                let key = self.proving.crates[of].plan.steps[step].key;
                let kept = match &done {
                    Done::Read(bytes) => verdict_of(bytes, &key),
                    // A verdict that cannot be read is one not kept: the step is run.
                    _ => None,
                };
                match kept {
                    Some(output) => {
                        // clone: Lean's words, for the next crate's same step
                        self.accepted.insert(key, output.clone());
                        self.proving.crates[of].ran[step] = Some(Ran::Accepted(output));
                    }
                    None => {
                        if let Err(why) = self.run(step, effects) {
                            return self.fail(why, out);
                        }
                    }
                }
            }
            (Flight::Lean { step, late, .. }, done) => {
                let of = self.at;
                let ran = match done {
                    Done::Exited(exit) => {
                        let mut output = String::from_utf8_lossy(&exit.stdout).into_owned();
                        output.push_str(&String::from_utf8_lossy(&exit.stderr));
                        match (late, exit.code) {
                            (true, _) => Ran::TimedOut,
                            (false, Some(0)) => Ran::Accepted(output),
                            (false, Some(_)) => Ran::Rejected(output),
                            (false, None) => Ran::Rejected(format!(
                                "Lean was ended by signal {}:\n{output}",
                                exit.signal.unwrap_or(0)
                            )),
                        }
                    }
                    other => {
                        // Lean that cannot be started is no verdict on a proof: no sandbox
                        // could be made, or its file is gone.
                        let module = &self.proving.crates[of].plan.steps[step].module;
                        return self.fail(
                            format!("Lean could not be run on `{module}` in blaze's sandbox: {}", said(&other)),
                            out,
                        );
                    }
                };
                let key = self.proving.crates[of].plan.steps[step].key;
                if let Ran::Accepted(output) = &ran {
                    // clone: Lean's words, kept under the step's key
                    self.accepted.insert(key, output.clone());
                    self.keep(of, step, key, output.clone(), effects);
                }
                #[cfg(test)]
                {
                    if self.plant == Plant::KeepRefused && matches!(ran, Ran::Rejected(_)) {
                        let kept = self.kept(of, step);
                        self.write_verdict(&kept, &key, "", effects);
                    }
                }
                self.proving.crates[of].ran[step] = Some(ran);
            }
            (Flight::Order { kept, key, output }, Done::Ordered) => {
                self.write_verdict(&kept, &key, &output, effects);
            }
            // A module that could not be ordered leaves no verdict: its step is run again.
            (Flight::Order { .. }, _) => {}
            (Flight::Other, Done::Fired) => {
                self.ticking = false;
                self.ticks += 1;
                let limit = DEADLINE.as_secs() / TICK.as_secs();
                let ticks = self.ticks;
                let mut late = Vec::new();
                for (tag, flight) in &mut self.flight {
                    match flight {
                        Flight::Lean { since, late: was, .. } if !*was && ticks - *since >= limit => {
                            *was = true;
                            late.push(*tag);
                        }
                        Flight::Version { since } if ticks - *since == limit => late.push(*tag),
                        _ => {}
                    }
                }
                for tag in late {
                    effects.push(Effect::Kill { tag: Tag(tag) });
                }
                if self.running() > 0 {
                    self.tick(effects);
                }
            }
            // A verdict kept, or not: the next check runs the step again where it was not.
            (Flight::Other, _) => {}
        }
        self.go(effects, out)
    }

    /// The check is cancelled: every Lean in flight is killed, and it ends, not ok and with
    /// no answer, once they have.
    fn stop(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        self.cancelled = true;
        let running: Vec<u64> = self
            .flight
            .iter()
            .filter(|(_, it)| matches!(it, Flight::Lean { .. } | Flight::Version { .. }))
            .map(|(tag, _)| *tag)
            .collect();
        for tag in &running {
            effects.push(Effect::Kill { tag: Tag(*tag) });
        }
        match running.is_empty() {
            true => Some(false),
            false => None,
        }
    }
}

impl Runs<Real> for Checking {
    fn start(
        &mut self,
        _store: &mut Store,
        _writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        _placed: &dyn Fn(&str) -> Option<[u8; 32]>, // dyn: as `Runs`'s; Lean is no placed output
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        self.begin(effects, out)
    }

    fn owns(&self, tag: Tag) -> bool {
        self.flight.contains_key(&tag.0)
    }

    fn complete(
        &mut self,
        completion: Completion,
        _store: &mut Store,
        _writer: Option<&mut dyn Asks>, // dyn: as `Runs`'s
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        self.told(completion, effects, out)
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        self.stop(effects)
    }

    /// A check puts nothing into the store: its verdicts are files beside the modules.
    fn take_puts(&mut self) -> RunPuts {
        RunPuts::default()
    }
}

/// How an effect that did not go as asked went, for an error.
fn said(done: &Done) -> String {
    match done {
        Done::Failed(error) => error.to_string(),
        other => format!("{other:?}"),
    }
}

/// How a run of Lean that was asked its version ended, for the refusal: its exit, what it
/// printed, and the end of what it wrote to standard error, which is where a loader that
/// could not start the program says why.
fn ended(exit: &blaze_core::Exit) -> String {
    let how = match (exit.code, exit.signal) {
        (Some(code), _) => format!("exited with {code}"),
        (None, Some(signal)) => format!("was ended by signal {signal}"),
        (None, None) => "ended with no exit code".to_owned(),
    };
    let printed = String::from_utf8_lossy(&exit.stdout);
    let errors = String::from_utf8_lossy(&exit.stderr);
    let errors = errors.trim();
    // The end of it: the last line is the one that says what stopped it.
    let from = errors.char_indices().rev().nth(599).map_or(0, |(at, _)| at);
    match errors.is_empty() {
        true => format!("{how} and printed `{}`", printed.trim()),
        false => format!("{how}, printed `{}` and wrote `{}` to standard error", printed.trim(), &errors[from..]),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, rc::Rc};

    use blaze_core::{
        Driver as _, Exit,
        sim::{self, Faults, Sim},
        wait::Deadline,
    };
    use blaze_sim::{Broke, Config, Tape};
    use hir_ty_lean::translate::Shape;

    use super::*;

    const LEAN: &str = "/lean/bin/lean";

    /// What Lean does with a module.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Does {
        Accept,
        Refuse,
        /// It does not end by the deadline.
        Late,
        /// Accepted, and of a check, on a proof with a `sorry`.
        Sorry,
    }

    /// One crate of a world: its name, its theorems as emitted and its proof files.
    #[derive(Clone, Debug)]
    struct Krate {
        name: String,
        theorems: Vec<Emitted>,
        proofs: BTreeMap<String, String>,
    }

    /// Crates to check, what Lean does with each module, in what order and how long.
    #[derive(Debug)]
    struct World {
        crates: Vec<Krate>,
        /// By module; a module not here is accepted.
        does: BTreeMap<String, Does>,
        seed: u64,
        /// Whether the power is cut between the two checks.
        crash: bool,
    }

    fn module(function: &str, statement: bool, imports: Vec<Emitted>) -> Emitted {
        let name = function.replace("::", ".");
        let mut text = String::from("-- a test's model\nimport Blaze\n");
        for import in &imports {
            text.push_str(&format!("import {}\n", import.module));
        }
        text.push_str(&format!("def {name} := true\n"));
        Emitted {
            function: function.to_owned(),
            module: format!("BlazeModel.{name}"),
            statement: statement.then(|| format!("{name}.spec.statement")),
            name,
            text,
            clauses: Vec::new(),
            shape: Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false },
            imports,
        }
    }

    fn plan_of(krate: &Krate) -> Plan {
        plan::plan(Ask { theorems: &krate.theorems, proofs: &krate.proofs })
            .unwrap_or_else(|error| panic!("{error}"))
    }

    impl World {
        /// A world from the tape. The calm one is one crate with one theorem, proved in one
        /// file that imports its model alone, every module accepted and no power cut.
        fn draw(tape: &Tape) -> World {
            let mut crates = Vec::new();
            let mut does = BTreeMap::new();
            for name in ["k", "j"].iter().take(1 + tape.below(2, "a second crate") as usize) {
                let upper = name.to_uppercase();
                let mut theorems = Vec::new();
                let mut proofs: BTreeMap<String, String> = BTreeMap::new();
                for at in 0..1 + tape.below(3, "more theorems") {
                    let part = tape.below(2, "the theorem's Rust module");
                    let helper = match tape.coin("it calls a helper") {
                        true => vec![module(&format!("{name}::helper"), false, Vec::new())],
                        false => Vec::new(),
                    };
                    let theorem = module(&format!("{name}::m{part}::t{at}"), true, helper);
                    // The proof file of its Rust module, which imports its model.
                    let file = proofs.entry(format!("{upper}.M{part}")).or_default();
                    file.push_str(&format!("import {}\n", theorem.module));
                    theorems.push(theorem);
                }
                let lemmas = tape.coin("a file of lemmas");
                for (file, text) in proofs.iter_mut() {
                    if tape.coin("the proof file imports Blaze.Auto") {
                        text.push_str("import Blaze.Auto\n");
                    }
                    if lemmas && file.ends_with("M0") {
                        text.push_str(&format!("import {upper}.Lemmas\n"));
                    }
                }
                if lemmas {
                    proofs.insert(format!("{upper}.Lemmas"), "import Blaze\n".to_owned());
                }
                if tape.one_in(8, "a proof file is missing") {
                    proofs.remove(&format!("{upper}.M1"));
                }
                let krate = Krate { name: (*name).to_owned(), theorems, proofs };
                for step in &plan_of(&krate).steps {
                    // The library's modules are every crate's: what the first crate drew.
                    if does.contains_key(&step.module) {
                        continue;
                    }
                    let outcomes = match step.kind {
                        plan::Kind::Library => 60,
                        _ => 9,
                    };
                    let does_it = match (step.kind, tape.below(outcomes, "what Lean does with the module")) {
                        (_, 1) => Does::Refuse,
                        (_, 2) => Does::Late,
                        (plan::Kind::Check, 3) => Does::Sorry,
                        _ => Does::Accept,
                    };
                    // clone: a test's names
                    does.insert(step.module.clone(), does_it);
                }
                crates.push(krate);
            }
            // The simulated machine is seeded: its order of completions is not yet the tape's.
            let seed = tape.next_u64("the machine's seed");
            World { crates, does, seed, crash: tape.one_in(3, "the power is cut between the checks") }
        }

        fn does(&self, module: &str) -> Does {
            self.does.get(module).copied().unwrap_or(Does::Accept)
        }

        /// The reference model: the plan run one step at a time, in order, each module doing
        /// what the world says.
        fn reference(&self, plan: &Plan) -> Vec<Ran> {
            let mut ran = vec![Ran::NotRun; plan.steps.len()];
            for at in plan::wanted(plan) {
                let step = &plan.steps[at];
                if step.needs.iter().any(|need| !matches!(ran[*need], Ran::Accepted(_))) {
                    continue;
                }
                ran[at] = match self.does(&step.module) {
                    Does::Accept => Ran::Accepted(said(&step.module, false)),
                    Does::Sorry => Ran::Accepted(said(&step.module, true)),
                    Does::Refuse => Ran::Rejected(refusal(&step.file())),
                    Does::Late => Ran::TimedOut,
                };
            }
            ran
        }

        /// What a check must answer: nothing where no proof could be checked, else each
        /// theorem with what was found of it; and the modules Lean accepts on the way.
        fn expected(&self) -> (Option<Vec<(String, String)>>, BTreeSet<String>) {
            let mut found = Vec::new();
            let mut accepted = BTreeSet::new();
            for krate in &self.crates {
                let plan = plan_of(krate);
                let ran = self.reference(&plan);
                for (step, ran) in plan.steps.iter().zip(&ran) {
                    if matches!(ran, Ran::Accepted(_)) {
                        // clone: a test's names
                        accepted.insert(step.module.clone());
                    }
                }
                let Ok(verdicts) = plan::conclude(&plan, &ran) else { return (None, accepted) };
                for verdict in verdicts {
                    found.push(match verdict {
                        Verdict::Proved { function, axioms, .. } => (function, format!("proved on {axioms:?}")),
                        Verdict::Open { function, why, .. } => (function, format!("open: {why}")),
                        Verdict::ModelBroken { function, module, .. } => (function, format!("model broken: {module}")),
                    });
                }
            }
            found.sort();
            (Some(found), accepted)
        }
    }

    /// What Lean prints of a module it accepts: of a check, the theorem's axioms.
    fn said(module: &str, sorry: bool) -> String {
        match module.strip_prefix("BlazeCheck.") {
            Some(name) => format!(
                "'{name}.spec.checked' depends on axioms: [propext{}]\n",
                if sorry { ", sorryAx" } else { "" }
            ),
            None => String::new(),
        }
    }

    fn refusal(file: &str) -> String {
        format!("{file}:1:0: error: refused\n")
    }

    /// The simulated Lean: what the world says of the module its last argument names.
    fn lean(does: Rc<BTreeMap<String, Does>>) -> impl FnMut(&Command, &mut sim::Rng) -> sim::Ran + 'static {
        move |command, rng| {
            let exit = |code: i32, stdout: String| Exit {
                code: Some(code),
                signal: None,
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
                traced: None,
                peak_memory: 0,
            };
            let last = command.args.last().and_then(|it| it.to_str()).unwrap_or("");
            if last == "--version" {
                let version = format!("Lean (version {LEAN_VERSION}, simulated)\n");
                return sim::Ran { exit: exit(0, version), takes: Duration::from_millis(5) };
            }
            let module = last.trim_end_matches(".lean").replace('/', ".");
            let takes = Duration::from_millis(1 + rng.below(3000));
            match does.get(&module).copied().unwrap_or(Does::Accept) {
                Does::Accept => sim::Ran { exit: exit(0, said(&module, false)), takes },
                Does::Sorry => sim::Ran { exit: exit(0, said(&module, true)), takes },
                Does::Refuse => sim::Ran { exit: exit(1, refusal(last)), takes },
                Does::Late => sim::Ran { exit: exit(0, said(&module, false)), takes: DEADLINE * 2 },
            }
        }
    }

    fn oracle(why: String) -> Broke {
        Broke::Oracle(why)
    }

    /// One check of `world` on `sim`, watched: its answer and the modules it gave Lean, in
    /// order. `earlier` are the modules an earlier check accepted, whose verdicts it may find.
    fn check(
        sim: &mut Sim,
        world: &World,
        plant: Plant,
        earlier: &BTreeSet<String>,
    ) -> Result<(Proven, Vec<String>), Broke> {
        // Each module's imports that are steps, for the watch below.
        let mut needs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut crates = Vec::new();
        for krate in &world.crates {
            let plan = plan_of(krate);
            for step in &plan.steps {
                // clone: a test's names
                let of = step.needs.iter().map(|need| plan.steps[*need].module.clone()).collect();
                needs.insert(step.module.clone(), of);
            }
            crates.push(Asked {
                // clone: a test's names
                name: krate.name.clone(),
                proofs: PathBuf::from(format!("/w/{}/lean", krate.name)),
                ran: plan.steps.iter().map(|_| None).collect(),
                wanted: plan::wanted(&plan),
                plan,
            });
        }
        let proving = Proving {
            emit: false,
            sandbox_off: true,
            out: PathBuf::from("/b/lean"),
            records: Vec::new(),
            subset: None,
            crates,
        };
        let mut checking = Checking::new(proving, Some(LEAN), 1 << 40);
        checking.plant = plant;
        let (mut effects, mut out, mut done) = (Vec::new(), Vec::new(), Vec::new());
        let mut given: Vec<String> = Vec::new();
        let mut running: BTreeMap<u64, String> = BTreeMap::new();
        let mut accepted: BTreeSet<String> = BTreeSet::new();
        let mut ended = checking.begin(&mut effects, &mut out);
        let mut turns = 0u32;
        while ended.is_none() {
            turns += 1;
            if turns > 200_000 {
                return Err(oracle("the check has not ended in 200,000 turns".to_owned()));
            }
            for effect in &effects {
                let Effect::Spawn { command, tag } = effect else { continue };
                let last = command.args.last().and_then(|it| it.to_str()).unwrap_or("");
                if last == "--version" {
                    continue;
                }
                let module = last.trim_end_matches(".lean").replace('/', ".");
                if given.contains(&module) {
                    return Err(oracle(format!("`{module}` was given to Lean twice in one check")));
                }
                for need in needs.get(&module).into_iter().flatten() {
                    if !accepted.contains(need) && !earlier.contains(need) {
                        return Err(oracle(format!(
                            "`{module}` was given to Lean before `{need}`, which it imports, was accepted"
                        )));
                    }
                }
                // clone: a test's names
                given.push(module.clone());
                running.insert(tag.0, module);
            }
            sim.submit(&mut effects);
            if sim.poll(&mut done, Deadline::NEVER) == 0 {
                return Err(oracle("the check waits with nothing in flight".to_owned()));
            }
            for completion in done.drain(..) {
                if let (Some(module), Done::Exited(exit)) = (running.remove(&completion.tag.0), &completion.done)
                    && exit.code == Some(0)
                {
                    accepted.insert(module);
                }
                if ended.is_none() && checking.owns(completion.tag) {
                    ended = checking.told(completion, &mut effects, &mut out);
                }
            }
        }
        // What it asked for as it ended (verdicts being written) is carried out, as the
        // daemon's driver carries it out.
        sim.submit(&mut effects);
        while sim.poll(&mut done, Deadline::NEVER) > 0 {
            done.clear();
        }
        if ended != Some(true) {
            return Err(oracle(format!("the check ended {ended:?}, where it always answers")));
        }
        match out.as_slice() {
            // clone: the answer, once
            [Response::Proven(proven)] => Ok((proven.clone(), given)),
            other => Err(oracle(format!(
                "the check answered {} responses, where it gives one `Proven`",
                other.len()
            ))),
        }
    }

    /// `proven` against what the world expects.
    fn same(proven: &Proven, expected: &Option<Vec<(String, String)>>) -> Result<(), String> {
        let Some(expected) = expected else {
            return match &proven.failed {
                Some(_) => Ok(()),
                None => Err("Lean refused a module of the library and the check answered as if it had checked".to_owned()),
            };
        };
        if let Some(why) = &proven.failed {
            return Err(format!("the check failed where it had proofs to check: {why}"));
        }
        let mut found: Vec<(String, String)> = Vec::new();
        for record in &proven.records {
            found.push(match record {
                // clone: a test's names
                ProofRecord::Proved { function, axioms, .. } => (function.clone(), format!("proved on {axioms:?}")),
                ProofRecord::Open { function, why, .. } => (function.clone(), format!("open: {why}")),
                ProofRecord::ModelBroken { function, module, .. } => {
                    (function.clone(), format!("model broken: {module}"))
                }
                other => return Err(format!("a record that is no verdict: {other:?}")),
            });
        }
        found.sort();
        match &found == expected {
            true => Ok(()),
            false => Err(format!("it found\n{found:#?}\nwhere the plan run in order finds\n{expected:#?}")),
        }
    }

    fn holds(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let world = World::draw(tape);
        let mut sim = Sim::new(world.seed, Faults::default());
        // clone: the simulated Lean's own copy of what each module does
        sim.program(LEAN, lean(Rc::new(world.does.clone())));
        let (expected, accepted) = world.expected();
        let (first, _) = check(&mut sim, &world, plant, &BTreeSet::new())?;
        same(&first, &expected).map_err(|broke| oracle(format!("the first check: {broke}")))?;
        if world.crash {
            sim.crash();
        }
        let (second, given) = check(&mut sim, &world, plant, &accepted)?;
        same(&second, &expected).map_err(|broke| oracle(format!("the second check: {broke}")))?;
        if !world.crash
            && expected.is_some()
            && let Some(again) = given.iter().find(|it| accepted.contains(*it))
        {
            return Err(oracle(format!(
                "the first check accepted `{again}` and the second gave it to Lean again"
            )));
        }
        Ok(())
    }

    /// The refinement: for any crates, proof files, and outcome and timing of Lean on each
    /// module, a check answers as its plan run one step at a time in order does
    /// ([`World::reference`]); it gives Lean no module twice, and none before every module
    /// it imports was accepted; and a second check of the same tree answers the same and,
    /// with no power cut between, gives Lean no module the first accepted. Its controls are
    /// `the_oracle_catches_a_step_started_before_what_it_imports` and
    /// `the_oracle_catches_a_refused_step_kept_as_accepted`.
    #[test]
    fn a_check_answers_as_its_plan_run_in_order_and_runs_nothing_twice() {
        blaze_sim::check(Config::cases(300), |tape| holds(tape, Plant::None));
    }

    #[test]
    fn the_oracle_catches_a_step_started_before_what_it_imports() {
        blaze_sim::control(
            Config::cases(300),
            Plant::None,
            Plant::BeforeNeeds,
            "which it imports, was accepted",
            holds,
        );
    }

    #[test]
    fn the_oracle_catches_a_refused_step_kept_as_accepted() {
        blaze_sim::control(Config::cases(300), Plant::None, Plant::KeepRefused, "the second check", holds);
    }

    /// A theorem with no proof file has its model compiled all the same, once: the first check
    /// gives Lean both models and answers the one Lean accepts open for want of its proof file
    /// and the one it refuses as blaze's bug, at Lean's place; the second check of the same
    /// tree answers the same and gives Lean the accepted model zero times. (The refused one is
    /// run again: a refusal leaves no verdict, `the_oracle_catches_a_refused_step_kept_as_accepted`.)
    #[test]
    fn a_model_without_a_proof_file_is_compiled_once_and_its_verdict_found_after() {
        let (good, bad) = ("BlazeModel.k.m.t0", "BlazeModel.k.m.t1");
        let theorems = vec![module("k::m::t0", true, Vec::new()), module("k::m::t1", true, Vec::new())];
        let krate = Krate { name: "k".to_owned(), theorems, proofs: BTreeMap::new() };
        let does = BTreeMap::from([(bad.to_owned(), Does::Refuse)]);
        let world = World { crates: vec![krate], does, seed: 7, crash: false };
        let mut sim = Sim::new(world.seed, Faults::default());
        // clone: the simulated Lean's own copy of what each module does
        sim.program(LEAN, lean(Rc::new(world.does.clone())));
        let (_, accepted) = world.expected();
        let (first, given) = check(&mut sim, &world, Plant::None, &BTreeSet::new()).unwrap_or_else(|it| panic!("{it:?}"));
        for model in [good, bad] {
            assert_eq!(given.iter().filter(|it| *it == model).count(), 1, "{model} in {given:?}");
        }
        assert_eq!(first.failed, None);
        match first.records.as_slice() {
            [ProofRecord::Open { function, why, .. }, ProofRecord::ModelBroken { function: broken, module, at, error }] => {
                assert_eq!((function.as_str(), broken.as_str(), module.as_str()), ("k::m::t0", "k::m::t1", bad));
                assert!(why.starts_with("there is no proof file"), "{why}");
                assert_eq!((at.as_str(), error.as_str()), ("/b/lean/lib/BlazeModel/k/m/t1.lean:1:0", "error: refused"));
            }
            other => panic!("{other:?}"),
        }
        assert!(accepted.contains(good) && !accepted.contains(bad), "{accepted:?}");
        let (second, given) = check(&mut sim, &world, Plant::None, &accepted).unwrap_or_else(|it| panic!("{it:?}"));
        assert_eq!(second, first);
        assert_eq!(given.iter().filter(|it| *it == good).count(), 0, "{given:?}");
        assert!(given.iter().all(|it| !accepted.contains(it)), "{given:?}");
    }

    /// A verdict is taken only where it is this key's and whole: one of another key, one cut
    /// short by a crash, and a file of the layout before (the key alone on its first line) are
    /// none, so their steps are run.
    #[test]
    fn a_foreign_torn_or_older_verdict_is_no_verdict() {
        let key = ContentId::of("blaze v2 cargo test build", b"a step");
        let other = ContentId::of("blaze v2 cargo test build", b"another step");
        let output = "'k.t.spec.checked' depends on axioms: [propext]\n";
        let kept = verdict_file(&key, output);
        assert_eq!(verdict_of(&kept, &key).as_deref(), Some(output));
        assert_eq!(verdict_of(&kept, &other), None);
        for cut in 0..kept.len() {
            assert_eq!(verdict_of(&kept[..cut], &key), None, "cut at {cut}");
        }
        assert_eq!(verdict_of(format!("{key:x}\n{output}").as_bytes(), &key), None);
        assert_eq!(verdict_of(b"", &key), None);
        // An accepted module that printed nothing has a verdict too.
        assert_eq!(verdict_of(&verdict_file(&key, ""), &key).as_deref(), Some(""));
    }
}
