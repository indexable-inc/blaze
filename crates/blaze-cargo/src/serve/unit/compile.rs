//! One unit's compile ([`Front::compile`](crate::serve::work::Front::compile)): its
//! diagnostics as rustc gives them and cargo shows them, then, unless it is a check, its code
//! compiled and linked by blaze, and every file cargo would write for it. Each of its
//! [`Phase`]s is a node of the plane of its own, timed by what it does: what one made waits
//! for the next in the unit's [`Phases`].
//!
//! A unit's result is kept in the store as its record, under what it was made from: the
//! front end's identity of the unit's inputs (its sources, cfgs, env, its dependencies' and
//! their build scripts' results, deeply), its plan, its profile and target, the toolchain and
//! blaze's own code ([`Toolchain::id`](blaze_codegen::settings::Toolchain)), the bytes of a
//! `-C profile-use` profile, the goal and the colour of its diagnostics. A unit whose record and outputs are all in the store is fresh:
//! its diagnostics are replayed as cargo replays a fresh unit's, and nothing runs.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_api::{ApiError, Artifact, Diagnostic, Level};
use blaze_cas::{Key, Kind};
use blaze_codegen::settings::Pgo;
use blaze_diagnostics::{Diag, Options, lints::CommandLine};
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use blaze_lower::{ProcMacroLoadResult, ProcMacroLoadingError};
use blaze_profile::span::{self, Name};
use blaze_units::schema::{Mode, Unit};
use hir_ty_codegen::v2::BodyChecks;
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};

use super::{
    Job, Real, UnitReader, artifact,
    checks::Checking,
    checks_only, graph,
    codegen::{self, Makes},
    files::Mounted,
    say, script,
};
use crate::serve::work::{Compiled, Failed, Goal, Linking, Output, Phase, Put};

type Image = crate::serve::work::Image<Real>;

/// What a compiled unit made that later work of the job reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Made {
    /// Its main file and content: a build script's program, a test binary, a library.
    pub(crate) main: Option<(String, ContentId)>,
    /// A test binary's tests, in the harness's order.
    pub(crate) tests: Vec<TestOf>,
    /// What the binary does depends on beside the code it reaches: its target, toolchain,
    /// codegen settings and panic strategy ([`blaze_usercode::test::Suite::context`]).
    pub(crate) context: Option<ContentId>,
    /// Every instance its codegen compiled or found, with the code hash it was made from, as
    /// [`blaze_api::Response::CodegenInputs`] lists them: sorted by key.
    pub(crate) inputs: Vec<(ContentId, ContentId)>,
    /// A test binary's execution table, made beside its link
    /// ([`super::test::image::table`]), which its tests are selected by: kept with its record,
    /// so a replayed binary selects by the table it was linked with.
    pub(crate) table: Option<blaze_codegen::ExecTable>,
}

/// A test of a test binary, as selection sees it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct TestOf {
    pub(crate) name: String,
    pub(crate) ignore: bool,
    /// `#[should_panic]`, with its `expected` message if it gives one.
    pub(crate) should_panic: Option<Option<String>>,
    pub(crate) bench: bool,
    /// It reaches the outside world, as codegen marks it: it always runs.
    pub(crate) outside: bool,
    /// What it reaches, sorted and each once: every code hash, argument and static value
    /// (`Built::reached`), and a hash of each span table of its binary.
    pub(crate) reached: Vec<CodeHash>,
}

/// A unit's result as the store keeps it, after the [`header`]: what a replay gives back.
#[derive(Deserialize)]
struct Record {
    ok: bool,
    diagnostics: Vec<Diagnostic>,
    outputs: Vec<(String, Key, bool)>,
    artifact: Option<Artifact>,
    made: Made,
}

/// [`Record`] as it is written, borrowing what the compile made rather than copying it.
#[derive(Serialize)]
struct RecordRef<'a> {
    ok: bool,
    diagnostics: &'a [Diagnostic],
    outputs: Vec<(&'a str, Key, bool)>,
    artifact: Option<&'a Artifact>,
    made: &'a Made,
}

const RECORD: &str = "blaze v2 cargo unit record 4";
const OUTPUT: &str = "blaze v2 cargo output 1";
/// A span table's anchor and content, as a test's selection holds it.
const SPANS: &str = "blaze v2 cargo test spans 1";
/// A test binary's context with the native inputs its link read, in link order.
const NATIVES: &str = "blaze v2 cargo test natives 1";

/// The record's fixed header, which `fresh` reads where the store has it, allocating nothing:
/// `MAGIC`, the count of outputs and of native inputs (`u32` LE each), each output's key
/// (32 bytes), then each native input's BLAKE3 hash (32 bytes), its path's length (`u32` LE)
/// and its path. The CBOR of [`Record`] follows.
pub(crate) mod header {
    use blaze_format::cursor::{Put as _, Reader};

    pub(crate) const MAGIC: &[u8; 4] = b"BZU1";

    pub(crate) fn write(out: &mut Vec<u8>, outputs: &[blaze_cas::Key], natives: &[(String, blaze_ids::ContentId)]) {
        let paths: usize = natives.iter().map(|it| it.0.len()).sum();
        out.reserve(12 + 32 * outputs.len() + 36 * natives.len() + paths);
        out.put_bytes(MAGIC);
        out.put_u32(outputs.len() as u32);
        out.put_u32(natives.len() as u32);
        for key in outputs {
            out.put_bytes(&key.0.0);
        }
        for (path, hash) in natives {
            out.put_bytes(&hash.0);
            out.put_bytes_u32(path.as_bytes());
        }
    }

    /// Whether every output is in the store and every native input is as it was, and where the
    /// CBOR body starts; `None` for a record that is not, or a header that is cut short.
    pub(crate) fn check(
        bytes: &[u8],
        has: impl Fn(&blaze_cas::Key) -> bool,
        file: &mut impl FnMut(&std::path::Path) -> Option<[u8; 32]>,
    ) -> Option<usize> {
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok()? != MAGIC {
            return None;
        }
        let outputs = read.u32().ok()?;
        let natives = read.u32().ok()?;
        for _ in 0..outputs {
            let key = blaze_cas::Key(blaze_ids::ContentId(*read.array().ok()?));
            if !has(&key) {
                return None;
            }
        }
        for _ in 0..natives {
            let hash: [u8; 32] = *read.array().ok()?;
            let path = read.str_u32().ok()?;
            if file(std::path::Path::new(path))? != hash {
                return None;
            }
        }
        Some(read.at())
    }
}

/// The key of `unit`'s record: none while the front end can't say what the unit's inputs are.
/// Worked out once per job and unit, once the writer has set the unit's identity; before
/// that, none and nothing memoized.
fn record_key(job: &Job, unit: usize) -> Result<Option<Key>, ApiError> {
    if let Some(key) = job.keys.get(unit).and_then(|it| it.get()) {
        return Ok(*key);
    }
    let Some(identity) = job.identities.get(unit).and_then(|it| it.get()) else { return Ok(None) };
    let key = match identity {
        Some(identity) => Some(work_out_key(job, unit, *identity)?),
        None => None,
    };
    if let Some(slot) = job.keys.get(unit) {
        let _ = slot.set(key);
    }
    Ok(key)
}

/// `unit`'s identity in the front end, once the writer set it.
fn identity(job: &Job, unit: usize) -> Option<ContentId> {
    job.identities.get(unit).and_then(|it| it.get()).copied().flatten()
}

fn work_out_key(job: &Job, unit: usize, identity: ContentId) -> Result<Key, ApiError> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let toolchain = codegen::toolchain(job, unit).map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?;
    let goal = keyed_goal(job.input.goal, u);
    let runs = runs(job, unit);
    // An inert script's compile makes nothing, unless `BLAZE_ELIDE=verify` builds it to run it.
    let verified = u.target.is_custom_build()
        && !script::ported(job, unit)
        && script::verifies(job)?
        && script::inert(job, unit)?.is_some();
    let reached = reached_only(job, u)?;
    let mut hasher = Hasher::new(RECORD);
    hasher.id(&identity);
    // The unit, its plan and its run environment, each encoded into one buffer kept per
    // thread and hashed from it: no buffer allocated per unit once the first has grown it.
    SCRATCH.with_borrow_mut(|scratch| -> Result<(), ApiError> {
        hash_cbor(&mut hasher, scratch, &KeyedUnit::of(job, u)?, "unit", unit)?;
        hash_cbor(&mut hasher, scratch, plan, "plan", unit)?;
        hash_cbor(&mut hasher, scratch, &runs.env, "run environment", unit)
    })?;
    hasher.id(&toolchain.id).update(&[
        goal,
        u8::from(job.input.color),
        u8::from(job.input.short),
        u8::from(verified),
        u8::from(reached),
    ]);
    // The flags name a profile by its path; its bytes are the input.
    if let Pgo::Use(profile) = codegen::unit_pgo(job, unit).map_err(|what| ApiError::new(what, Some(u.target.name.clone())))? {
        hasher.id(&profile);
    }
    // The rendered diagnostics' columns: none is rustc's default, apart from every width.
    hasher.update(&job.input.width.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
    // How many refused functions its error names: every one apart from each count.
    hasher.update(&job.input.refused.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
    Ok(Key(hasher.finish()))
}

/// The goal as `unit`'s record key holds it. A build script's compile is the same program
/// whatever the job's goal ([`super::checks_only`] is false for it under every goal), so a
/// `blaze check` and a `blaze build` share its record; any other unit keys the goal.
pub(crate) fn keyed_goal(goal: Goal, unit: &Unit) -> u8 {
    if unit.mode == Mode::Build && unit.target.is_custom_build() {
        return 1;
    }
    match goal {
        Goal::Check => 0,
        Goal::Build => 1,
        Goal::Test => 2,
        Goal::Run => 3,
        Goal::Reach => 4,
    }
}

/// Whether `unit` is a build script elision found inert, or one of std's own that blaze has a
/// port of, so neither built nor run: under `BLAZE_ELIDE=verify` an inert one is built and run
/// as well; std's never is ([`script::ported`]).
fn elided_unbuilt(job: &Job, unit: usize) -> Result<bool, ApiError> {
    let custom_build = job.unit(unit)?.target.is_custom_build();
    let unverified = script::ported(job, unit) || !script::verifies(job)?;
    Ok(custom_build && unverified && script::inert(job, unit)?.is_some())
}

/// The variable that builds every dependency whole: `full`, for the differential run against a
/// reachable-only build (`harness/v2/reachable.py`); unset, only what the build's programs reach.
pub(crate) const DEPS: &str = "BLAZE_DEPS";

/// Why a crate [`reached_only`] has no check outcome or bodies of its own in the store.
pub(crate) const REACHED_ONLY: &str =
    "only what the build's programs reach of it is type-checked and compiled (`BLAZE_DEPS=full` checks it whole)";

/// Whether only what the build's programs reach of `unit`'s crate is type-checked and compiled
/// (docs/v2/plan/lanes/cold-v2/notes.md): a crate of the standard library, or of a package
/// cargo caps the lints of (a registry or git dependency), unless the job's [`DEPS`] is `full`.
/// Such a library runs no check and compiles no roots of its own: a program's walk, from roots
/// as complete as rustc's collector's, compiles what it reaches of it and type-checks those
/// bodies on the way, and a body it reaches that does not type-check stops that walk with an
/// error naming it. A workspace member and a path package are checked and compiled whole, as
/// cargo shows their diagnostics. Any other value of [`DEPS`] is an error naming it.
pub(crate) fn reached_only(job: &Job, unit: &Unit) -> Result<bool, ApiError> {
    match job.input.config.invocation.env.iter().find(|(name, _)| name == DEPS).map(|(_, value)| value.as_str()) {
        None => {}
        Some("full") => return Ok(false),
        Some(value) => {
            return Err(ApiError::new(
                // alloc: error path
                format!(
                    "{DEPS}={value} is not a value blaze takes: `full` checks and compiles every dependency whole, \
                     unset only what the build's programs reach of each"
                ),
                None,
            ));
        }
    }
    let package = job.package(unit)?;
    Ok(capped(unit.is_std, package.is_member, package.source.as_deref()))
}

/// Whether a unit's diagnostics are ones cargo never shows (lints capped, so only an error
/// could be seen): the standard library's, and any package that is neither a workspace member
/// nor at a path.
pub(crate) fn capped(is_std: bool, is_member: bool, source: Option<&str>) -> bool {
    is_std || !(is_member || local(source))
}

/// A unit as its record's key hashes it: each dependency by what it is, never by its index
/// in the graph, which another workspace's graph (or this one's after a dependency is added)
/// gives another number. What a dependency is made from is in the identity.
#[derive(Serialize)]
pub(crate) struct KeyedUnit<'a> {
    pkg_id: &'a str,
    target: &'a blaze_units::schema::Target,
    profile: &'a blaze_units::schema::Profile,
    platform: &'a Option<String>,
    mode: blaze_units::schema::Mode,
    features: &'a [String],
    is_std: bool,
    dependencies: Vec<KeyedDep<'a>>,
}

#[derive(Serialize)]
struct KeyedDep<'a> {
    pkg_id: &'a str,
    target: &'a str,
    mode: blaze_units::schema::Mode,
    platform: &'a Option<String>,
    extern_crate_name: &'a str,
    public: Option<bool>,
    noprelude: Option<bool>,
    nounused: Option<bool>,
}

impl<'a> KeyedUnit<'a> {
    fn of(job: &'a Job, u: &'a Unit) -> Result<KeyedUnit<'a>, ApiError> {
        KeyedUnit::among(u, |at| job.unit(at))
    }

    /// `u` of a graph whose unit `at` is `unit_at(at)`.
    pub(crate) fn among(u: &'a Unit, unit_at: impl Fn(usize) -> Result<&'a Unit, ApiError>) -> Result<KeyedUnit<'a>, ApiError> {
        // alloc: once per unit's key, a dependency each
        let mut dependencies = Vec::with_capacity(u.dependencies.len());
        for dep in &u.dependencies {
            let on = unit_at(dep.index)?;
            dependencies.push(KeyedDep {
                pkg_id: &on.pkg_id,
                target: &on.target.name,
                mode: on.mode,
                platform: &on.platform,
                extern_crate_name: &dep.extern_crate_name,
                public: dep.public,
                noprelude: dep.noprelude,
                nounused: dep.nounused,
            });
        }
        Ok(KeyedUnit {
            pkg_id: &u.pkg_id,
            target: &u.target,
            profile: &u.profile,
            platform: &u.platform,
            mode: u.mode,
            features: &u.features,
            is_std: u.is_std,
            dependencies,
        })
    }
}

fn hash_cbor<T: Serialize + ?Sized>(
    hasher: &mut Hasher,
    scratch: &mut Vec<u8>,
    value: &T,
    what: &str,
    unit: usize,
) -> Result<(), ApiError> {
    scratch.clear();
    blaze_protocol::cbor::append(scratch, value)
        .map_err(|why| ApiError::new(format!("encoding the {what} of unit {unit}: {why}"), None))?;
    hasher.part(scratch);
    Ok(())
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The machine's files as the process's table of them by `stat` hashes them: one `stat` for a
/// file hashed before, read only when its `stat` changed.
fn machine_file(disk: &mut blaze_usercode::inputs::Disk) -> impl FnMut(&std::path::Path) -> Option<[u8; 32]> + '_ {
    move |path| blaze_usercode::trace::Inputs::file(disk, path).ok().flatten()
}

/// Whether nothing of `unit` will be made again: its record and all it wrote are in the store,
/// and every native input it linked is as it was. Read in place: no decode, no allocation.
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    let Ok(Some(key)) = record_key(job, unit) else { return false };
    let guard = blaze_cas::enter();
    found(&job.input.store, &guard, &key).is_some() || (shared(job, unit) && found(&job.input.std, &guard, &key).is_some())
}

/// Whether `unit` is the standard library's, whose records are also in the shared std store
/// ([`crate::serve::work::JobInput::std`]).
fn shared(job: &Job, unit: usize) -> bool {
    job.unit(unit).is_ok_and(|it| it.is_std)
}

/// The record of `key` in `store`, every output it names in `store` and every native input
/// as it was, and where its CBOR body starts.
fn found<'g>(store: &'g blaze_cas::Reader, guard: &'g blaze_cas::Guard, key: &Key) -> Option<(blaze_cas::Entry<'g>, usize)> {
    let entry = store.get(guard, key).ok()??;
    let mut disk = blaze_usercode::inputs::Disk::new(Vec::new());
    let body = header::check(entry.bytes, |key| store.has(guard, key), &mut machine_file(&mut disk))?;
    Some((entry, body))
}

/// What `unit`'s last build made, as its record keeps it: none when the view's units were never
/// built, or were built from other code.
pub(crate) fn last_made(job: &Job, unit: usize) -> Option<Made> {
    kept(job, unit).map(|it| it.0.made)
}

/// Every instance the view's last build compiled or found, with the code hash it was made
/// from, sorted by key: each unit's from its record. A unit with no record, never built or
/// built from other code, is an error naming it.
pub(crate) fn codegen_inputs(job: &Job) -> Result<Vec<(ContentId, ContentId)>, Failed> {
    let mut inputs = Vec::new();
    for unit in 0..job.graph.len() {
        if job.graph.has(unit as u32, graph::flag::RUN) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let Some(made) = last_made(job, unit) else {
            let name = &job.unit(unit)?.target.name;
            return Err(ApiError::new(format!("`{name}` has no build of this view to list the codegen inputs of"), None).into());
        };
        inputs.extend(made.inputs);
    }
    inputs.sort_unstable();
    inputs.dedup();
    Ok(inputs)
}

/// The record of `unit` the store has, every output and native input of it as it was. A
/// standard library unit's the store has not is the shared std store's, another daemon's,
/// with the puts that copy it and every output it names into the store.
fn kept(job: &Job, unit: usize) -> Option<(Record, Vec<Put>)> {
    let key = record_key(job, unit).ok()??;
    let guard = blaze_cas::enter();
    if let Some((entry, body)) = found(&job.input.store, &guard, &key) {
        return Some((blaze_protocol::cbor::from_slice(&entry.bytes[body..]).ok()?, Vec::new()));
    }
    if !shared(job, unit) {
        return None;
    }
    let std = &job.input.std;
    let (entry, body) = found(std, &guard, &key)?;
    let record: Record = blaze_protocol::cbor::from_slice(&entry.bytes[body..]).ok()?;
    let mut puts = Vec::with_capacity(record.outputs.len() + 1); // alloc: once per std unit found shared
    for (_, output, _) in &record.outputs {
        let it = std.get(&guard, output).ok()??;
        // alloc: a copy of what another daemon made, once per output, for the store to own
        puts.push(Put { key: *output, kind: it.kind, bytes: it.bytes.to_vec() });
    }
    // alloc: the record's copy, once per std unit found shared
    puts.push(Put { key, kind: entry.kind, bytes: entry.bytes.to_vec() });
    Some((record, puts))
}

/// What std unit `unit` made, for the shared std store: its record `record` and every output
/// it names, from `compiled`'s puts or the store.
fn share(job: &Job, unit: usize, compiled: &Compiled, record: &Put) -> Result<Vec<Put>, ApiError> {
    let guard = blaze_cas::enter();
    let mut share = Vec::with_capacity(compiled.outputs.len() + 1); // alloc: once per std unit made
    for output in &compiled.outputs {
        let put = match compiled.puts.iter().find(|it| it.key == output.key) {
            // alloc: a copy for the shared store, once per output of a std unit made
            Some(put) => Put { key: put.key, kind: put.kind, bytes: put.bytes.clone() },
            None => match job.input.store.get(&guard, &output.key) {
                // alloc: a copy for the shared store, once per output of a std unit made
                Ok(Some(it)) => Put { key: output.key, kind: it.kind, bytes: it.bytes.to_vec() },
                Ok(None) => {
                    // alloc: an error, once
                    let what = format!("unit {unit}'s output {} is neither made now nor in the store", output.path);
                    return Err(ApiError::new(what, None));
                }
                Err(corrupt) => return Err(ApiError::new(format!("unit {unit}'s output {}: {corrupt}", output.path), None)),
            },
        };
        share.push(put);
    }
    // alloc: the record's copy, once per std unit made
    share.push(Put { key: record.key, kind: record.kind, bytes: record.bytes.clone() });
    Ok(share)
}

/// What a unit's phases made so far, for the phases after them ([`Phase`]), under the unit's
/// lock in [`Job::phases`]. A phase takes what it read only once nothing of it can give way,
/// so a phase posted again finds the unit as it was.
#[derive(Default)]
pub(crate) struct Phases {
    /// Its compile replayed from its record: the phases after [`Phase::Names`] make nothing.
    kept: Option<Compiled>,
    /// What [`Phase::Names`] found the unit does.
    does: Option<Does>,
    /// The check of every body of its crate, when one runs: [`Phase::Bodies`] runs it, and
    /// [`Phase::Codegen`] beside it is told what it reaches first and whether it failed
    /// ([`super::checks`]).
    checking: Option<Arc<Checking>>,
    /// The check's diagnostics, or those of the check served for it.
    diags: Option<Vec<Diag>>,
    /// The text of each file `diags` point into that no mount of the build holds, by its name
    /// in them ([`unmounted`]).
    unmounted: Vec<(String, String)>,
    /// What [`Phase::Codegen`] gave; none when the unit compiles no code.
    generated: Option<Generated>,
    /// The unit as [`Phase::Link`] left it, for [`Phase::Store`].
    linked: Option<Linked>,
}

/// What a unit does, as [`Phase::Names`] finds it.
#[derive(Clone, Copy)]
struct Does {
    /// A build script found to do nothing: neither built nor run.
    inert: bool,
    /// It compiles code of its own: it is no check, and not a library compiled only as a
    /// program reaches it.
    builds_code: bool,
}

/// What codegen gave: what the unit makes and the code generator's result, or why the unit
/// makes nothing codegen can say.
type Generated = Result<(Makes, Result<Result<codegen::Parts, Vec<String>>, String>), String>;

/// A unit as [`Phase::Link`] left it.
enum Linked {
    /// Replayed from its record, a proc macro's expanders loaded.
    Kept(Compiled),
    // alloc: once per unit made
    Made(Box<Making>),
}

/// What a unit made, before its outputs are hashed for the store.
struct Making {
    diagnostics: Vec<Diagnostic>,
    ok: bool,
    made: Made,
    /// No file at all: it failed, or it is a build script that does nothing.
    nothing: bool,
    /// Each file rustc would write: its path, bytes, and whether it is executable.
    files: Vec<(String, Vec<u8>, bool)>,
    /// What codegen put, the outputs to follow.
    puts: Vec<Put>,
    natives: Vec<(String, ContentId)>,
    links: Vec<(String, Linking)>,
    macros: Option<ProcMacroLoadResult>,
}

/// Phase `phase` of `unit`'s compile, reading the front end through `reader`; the compile
/// itself once its last, [`Phase::Store`], is done. `images` are the kept images
/// [`Phase::Link`] relinks. A proc macro's compile gives its expanders ([`Compiled::macros`]).
pub(crate) fn compile(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phase: Phase,
    images: &mut Vec<Image>,
) -> Result<Option<Compiled>, Failed> {
    let Some(slot) = job.phases.get(unit) else {
        // alloc: error path
        return Err(ApiError::new(format!("unit {unit} has no phases in the job"), None).into());
    };
    let of = job.units().graph.units.get(unit);
    let krate = Name::of(of.map_or("", |it| it.target.name.as_str()));
    // alloc: once per phase of a unit, the name its span carries
    let unit_name = Name::of(&of.map(span_unit).unwrap_or_default());
    let _span = match phase {
        Phase::Names => span::UnitNames { krate, unit: unit_name }.open(),
        Phase::Bodies => span::UnitBodies { krate, unit: unit_name }.open(),
        Phase::Codegen => span::UnitCodegen { krate, unit: unit_name }.open(),
        Phase::Link => span::UnitLink { krate, unit: unit_name }.open(),
        Phase::Store => span::UnitStore { krate, unit: unit_name }.open(),
    };
    match phase {
        Phase::Names => names(job, reader, unit, slot)?,
        Phase::Bodies => bodies(job, reader, unit, slot)?,
        Phase::Codegen => generate(job, reader, unit, slot)?,
        Phase::Link => link(job, reader, unit, &mut lock(slot), images)?,
        Phase::Store => return store(job, reader, unit, &mut lock(slot)).map(Some),
    }
    Ok(None)
}

/// A unit as its phase spans name it in `unit`: its package, the package's version and its
/// target, `serde v1.0.219 (build script)`, `blaze-api v0.1.0 (lib test)`. Every build script's
/// target is `build-script-build`, so the target's name alone (the spans' `crate`) can't tell
/// two apart.
pub(super) fn span_unit(unit: &Unit) -> String {
    let package = crate::cli::say::package_name(&unit.pkg_id);
    let version = crate::cli::say::package_version(&unit.pkg_id);
    let target = &unit.target;
    let kind = target.kind.first().map_or("", String::as_str);
    let named: std::borrow::Cow<'_, str> = match kind {
        _ if target.is_custom_build() && unit.mode == Mode::RunCustomBuild => "build script run".into(),
        _ if target.is_custom_build() => "build script".into(),
        _ if target.is_lib() => "lib".into(),
        kind => format!("{kind} \"{}\"", target.name).into(), // alloc: once per phase of a unit
    };
    // As cargo names a unit: a test or bench target's test is just the target.
    let test = (unit.mode == Mode::Test || unit.checked_as_test) && !matches!(kind, "test" | "bench");
    format!("{package} v{version} ({named}{})", if test { " test" } else { "" }) // alloc: once per phase of a unit
}

/// A unit's phases, each holding the lock while it changes them. Only [`Phase::Bodies`] and
/// [`Phase::Codegen`], side by side, ever wait on it, each only to read or leave a field.
fn lock(slot: &Mutex<Phases>) -> MutexGuard<'_, Phases> {
    // A phase that panicked took nothing it read: it changes the unit's only once it is done.
    slot.lock()
}

/// [`Phase::Names`]: the unit's record, replayed when the store keeps it; otherwise its
/// lint flags, its crate's def map, what it does, and its check: served from the store, none
/// (what a program reaches of the crate is type-checked by its walk), or of every body.
fn names(job: &Job, reader: &UnitReader, unit: usize, slot: &Mutex<Phases>) -> Result<(), Failed> {
    if let Some((record, puts)) = kept(job, unit) {
        let compiled = Compiled {
            diagnostics: record.diagnostics,
            ok: record.ok,
            puts,
            share: Vec::new(),
            outputs: record
                .outputs
                .into_iter()
                .map(|(path, key, executable)| Output { path, key, executable })
                .collect(),
            artifact: record.artifact.map(|it| Artifact { fresh: true, ..it }),
            macros: None,
            links: Vec::new(),
        };
        if let Some(made) = job.made.get(unit) {
            let _ = made.set(record.made);
        }
        lock(slot).kept = Some(compiled);
        return Ok(());
    }
    let u = job.unit(unit)?;
    let krate = crate_of(job, unit, u)?;
    options(job, unit, u)?;
    let db = reader.db();
    let _ = hir_def::nameres::crate_def_map(db, krate);
    let inert = elided_unbuilt(job, unit)? || script::pure::unbuilt(job, reader, unit)?;
    let reached = reached_only(job, u)?;
    // A library compiled only as a program reaches it has no code of its own to build, and a
    // proc macro's lib none that is loaded: its expanders run its MIR ([`macros`]).
    let makes = codegen::makes(u);
    let library_reached = reached && makes.as_ref().is_ok_and(|it| *it == Makes::Library);
    let proc_macro = makes.is_ok_and(|it| it == Makes::Shared { proc_macro: true });
    let builds_code = !inert && !checks_only(job.input.goal, u) && !library_reached && !proc_macro;
    let (checking, diags) = match blaze_diagnostics::cache::read(db, reader.checks(), krate, job.mounted.root()) {
        Some(Err(why)) => return Err(ApiError::new(why, Some(u.target.name.clone())).into()),
        // A dependency's check outcome from the store: no check runs, and codegen reads the
        // bodies' records served with it.
        Some(Ok(outcome)) => (None, Some(outcome.diags)),
        // alloc: none: an empty `Vec` holds no buffer
        None if reached => (None, Some(Vec::new())),
        // alloc: once per unit checked, shared by its check and its codegen
        None => (Some(Arc::new(Checking::new(db, hir::Crate::from(krate)))), None),
    };
    let unmounted = diags.as_deref().map_or_else(Vec::new, |diags| unmounted(reader, &job.mounted, diags));
    let mut phases = lock(slot);
    phases.does = Some(Does { inert, builds_code });
    (phases.checking, phases.diags, phases.unmounted) = (checking, diags, unmounted);
    Ok(())
}

/// The text of each file `diags` point into that no mount of the build holds (a build script's
/// output under `OUT_DIR`, read by an `include!`), as the database has it: the text their spans
/// were made against, which [`assemble`], with no database, can't read through `mounted`. Held
/// is a mount's file there, not a path under a mount's directory: `target/` is under the
/// workspace's, and the view has none of its outputs. A file the database has no text of is
/// left out, and [`assemble`] names it as unread.
fn unmounted(reader: &UnitReader, mounted: &Mounted, diags: &[Diag]) -> Vec<(String, String)> {
    let (db, paths) = (reader.db(), reader.paths());
    let names: Vec<&str> = blaze_diagnostics::emit::Sources::default()
        .unread(diags)
        .into_iter()
        // A mount's bytes that fail their check are left to `add_sources`, which adds them unread.
        .filter(|name| matches!(mounted.read(&mounted.absolute(name)), Ok(None)))
        .collect(); // alloc: the few names of a unit's diagnostics no mount holds
    if names.is_empty() {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let mut found: Vec<(String, base_db::FileId)> = paths
        .iter()
        .filter(|&(&file, _)| db.has_file_text(file))
        .map(|(&file, path)| (mounted.name(path), file))
        .filter(|(name, _)| names.contains(&name.as_str()))
        .collect(); // alloc: as above
    // The first file of a name by ID, whatever order the map is in.
    found.sort_by_key(|(name, file)| (name.clone(), file.index())); // clone: a few names, sorted once
    found.dedup_by(|a, b| a.0 == b.0);
    found
        .into_iter()
        // alloc: a file's text, once per file of a unit's diagnostics no mount holds
        .map(|(name, file)| (name, db.file_text(file).text(db).to_string()))
        .collect()
}

/// [`Phase::Bodies`]: the check of every body of the unit's crate, when one runs, its outcome
/// kept to be stored as a dependency crate's.
fn bodies(job: &Job, reader: &UnitReader, unit: usize, slot: &Mutex<Phases>) -> Result<(), Failed> {
    // clone: a refcount, once per unit checked
    let Some(checking) = lock(slot).checking.clone() else { return Ok(()) };
    let u = job.unit(unit)?;
    let krate = crate_of(job, unit, u)?;
    let (flags, options) = options(job, unit, u)?;
    let db = reader.db();
    let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
    let diags = checking.check(db, files, &options);
    if hir_def::deps::eligible(db, krate) {
        let paths = blaze_diagnostics::cache::Paths::of(db, krate);
        // alloc: the outcome encoded once more per dependency crate checked, for its report line
        let refused = blaze_diagnostics::cache::encode(&diags, &paths, job.mounted.root()).err().map(|it| it.to_string());
        // clone: once per dependency crate checked this build
        job.check_later(krate, flags, diags.clone(), refused);
    }
    let plan = job.plan(unit)?;
    let diags = match &plan.clippy {
        Some(clippy) => linted(db, krate, reader, job.mounted.root(), u, plan, clippy, options.lints, diags)?,
        None => diags,
    };
    let unmounted = unmounted(reader, &job.mounted, &diags);
    let mut phases = lock(slot);
    (phases.diags, phases.unmounted) = (Some(diags), unmounted);
    Ok(())
}

/// `diags`, the check's, with what clippy's lints and blaze's find in the unit's crate under
/// `cargo clippy`, at the levels of its lint flags and attributes, before rustc's closing lines,
/// which are made again to count them.
#[expect(clippy::too_many_arguments, reason = "the crate, where its files are named from, its plan and the check's")]
fn linted(
    db: &base_db::Db,
    krate: base_db::Crate,
    reader: &UnitReader,
    root: &str,
    u: &Unit,
    plan: &blaze_units::query::UnitPlan,
    clippy: &blaze_units::query::Clippy,
    command_line: CommandLine,
    mut diags: Vec<Diag>,
) -> Result<Vec<Diag>, ApiError> {
    let at = || Some(format!("clippy's lints of `{}`", u.target.name));
    let rust_version = plan.env.iter().find(|(name, _)| name == "CARGO_PKG_RUST_VERSION").map(|(_, it)| it.as_str());
    let file = clippy.conf.clone().map(|(path, text)| (std::path::PathBuf::from(path), text)); // clone: the file's text, once per unit linted
    let conf = blaze_lints::conf::Conf::of_text(file, rust_version.filter(|it| !it.is_empty()))
        .map_err(|why| ApiError::new(why, at()))?;
    let paths = reader.paths();
    // alloc: a file's name, once per file of the crate and per span that leaves its file
    let name_of = |file: base_db::FileId| paths.get(&file).map(|path| blaze_diagnostics::Files::relative(path, root).to_owned());
    let setup = blaze_lints::Setup { command_line, conf };
    let found = blaze_lints::check(db, hir::Crate::from(krate), &setup, &name_of)
        .map_err(|why| ApiError::new(why.to_string(), at()))?;
    let end = diags.iter().rposition(|it| !closing(it)).map_or(0, |at| at + 1);
    diags.truncate(end);
    diags.extend(found.into_iter().map(|it| it.diagnostic));
    let closing = blaze_diagnostics::emit::summary(diags.iter());
    diags.extend(closing);
    Ok(diags)
}

/// Whether `diag` is one of rustc's closing lines after a crate's diagnostics
/// ([`blaze_diagnostics::emit::summary`]'s).
fn closing(diag: &Diag) -> bool {
    diag.code.is_none()
        && diag.span.primary.is_empty()
        && (diag.level == blaze_diagnostics::model::Level::FailureNote || cargo_hides(&diag.message))
}

/// [`Phase::Codegen`]: the unit's functions compiled from its roots, when it compiles code,
/// told by its check beside it what fails.
fn generate(job: &Job, reader: &UnitReader, unit: usize, slot: &Mutex<Phases>) -> Result<(), Failed> {
    let (does, checking) = {
        let phases = lock(slot);
        // clone: a refcount, once per unit compiled
        (phases.does, phases.checking.clone())
    };
    let Some(Does { builds_code: true, .. }) = does else { return Ok(()) };
    let u = job.unit(unit)?;
    // dyn: `HirFrontend::with_checks` takes it; called once a level of the walk
    let checks = checking.as_deref().map(|it| it as &dyn BodyChecks);
    let generated = codegen::makes(u).map(|makes| {
        let generated = codegen::generate(job, reader, unit, &makes, &job.mounted, checks);
        (makes, generated)
    });
    // Its build may have stopped for the cancel, with an error that is not the unit's.
    if job.cancelled() {
        return Err(Failed::Again);
    }
    lock(slot).generated = Some(generated);
    Ok(())
}

/// [`Phase::Link`]: the unit's diagnostics as cargo shows them and, unless its check failed,
/// its images linked from what codegen made (relinking `images`) or a library's record; a
/// proc macro's expanders loaded. A unit that has no link phase has this done by its store.
fn link(job: &Job, reader: &UnitReader, unit: usize, phases: &mut Phases, images: &mut Vec<Image>) -> Result<(), Failed> {
    if let Some(ok) = phases.kept.as_ref().map(|it| it.ok) {
        let macros = macros(job, reader, unit, ok);
        if let Some(mut kept) = phases.kept.take() {
            kept.macros = macros;
            phases.linked = Some(Linked::Kept(kept));
        }
        return Ok(());
    }
    let mut making = assemble(job, reader, unit, phases, images)?;
    making.macros = macros(job, reader, unit, making.ok);
    if !making.nothing
        && let Some(Ok((_, Ok(Ok(parts))))) = &mut phases.generated
    {
        // Moved, not copied: the store keeps them as they are.
        let built = std::mem::take(&mut parts.built.puts);
        making.puts.extend(built.into_iter().map(|it| Put { key: it.key, kind: it.kind, bytes: it.bytes }));
    }
    if let Some(Ok((_, Ok(Ok(parts))))) = &mut phases.generated
        && !parts.built.memo_used.is_empty()
    {
        let used = std::mem::take(&mut parts.built.memo_used);
        if let Some(slot) = job.memo_used.get(unit) {
            // A unit links once a job: a second set has nothing the first lacks.
            let _ = slot.set(used);
        }
    }
    phases.linked = Some(Linked::Made(Box::new(making)));
    Ok(())
}

/// [`Phase::Store`]: each output of the unit hashed into one put, its record encoded, and its
/// compile.
fn store(job: &Job, reader: &UnitReader, unit: usize, phases: &mut Phases) -> Result<Compiled, Failed> {
    if phases.linked.is_none() {
        link(job, reader, unit, phases, &mut Vec::new())?;
    }
    if matches!(phases.linked, Some(Linked::Kept(_)))
        && let Some(Linked::Kept(kept)) = phases.linked.take()
    {
        return Ok(kept);
    }
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let package = job.package(u)?;
    // Read before the unit's is taken, so a give-way finds it as it was.
    let sources = crate_files(reader, crate_of(job, unit, u)?);
    let key = record_key(job, unit)?;
    let Some(Linked::Made(making)) = phases.linked.take() else {
        // alloc: error path
        return Err(ApiError::new(format!("unit {unit} (`{}`) is stored before it is linked", u.target.name), None).into());
    };
    let Making { diagnostics, ok, mut made, nothing, files, mut puts, natives, links, macros } = *making;
    let mut outputs = Vec::new();
    let mut artifact = None;
    if !nothing {
        let mounted = &job.mounted;
        // Each file's bytes go into the store once, moved, under their key; a copy cargo makes
        // of one (`target/debug/foo` of `deps/foo-0123`) is the same key.
        let mut keyed: Vec<(String, Key, bool)> = Vec::with_capacity(files.len());
        let mut output_keys: Vec<Key> = Vec::with_capacity(plan.outputs.len());
        let mut put_output = |puts: &mut Vec<Put>, bytes: Vec<u8>| {
            let key = Key(ContentId::of(OUTPUT, &bytes));
            if !output_keys.contains(&key) {
                output_keys.push(key);
                puts.push(Put { key, kind: Kind::Output, bytes });
            }
            key
        };
        let main_file = files.first().map(|it| it.0.clone());
        for (path, bytes, executable) in files {
            let key = put_output(&mut puts, bytes);
            keyed.push((path, key, executable));
        }
        outputs.reserve(plan.outputs.len());
        for path in &plan.outputs {
            let found = keyed
                .iter()
                .find(|it| it.0 == *path)
                .or_else(|| keyed.iter().find(|it| artifact::uplifted(&it.0, plan) == Some(path.as_str())));
            let (key, executable) = match found {
                Some((_, key, executable)) => (*key, *executable),
                None if path.ends_with(".d") => {
                    let text = dep_info(path, plan, main_file.as_deref(), &sources, mounted);
                    (put_output(&mut puts, text.into_bytes()), false)
                }
                // Debug info beside an image (`.dSYM`, `.pdb`, `.dwp`): blaze's images carry
                // their line tables, and write none beside them.
                None => continue,
            };
            outputs.push(Output { path: path.clone(), key, executable });
        }
        let main = artifact::main(u, plan);
        let main_key = main.as_ref().and_then(|main| outputs.iter().find(|it| it.path == *main)).map(|it| it.key);
        made.main = match (&main, main_key) {
            (Some(path), Some(key)) => Some((path.clone(), key.0)),
            _ => None,
        };
        artifact = match (&main, main_key) {
            (Some(path), Some(key)) => Some(artifact::artifact(u, plan, package, path, key.0, false, runs(job, unit))?),
            _ => None,
        };
        // The native code linked in is in no code hash a test reaches: a change of it runs them all.
        if let Some(context) = &mut made.context {
            *context = with_natives(*context, &natives);
        }
    }
    let mut compiled = Compiled { diagnostics, ok, puts, share: Vec::new(), outputs, artifact, macros, links };
    if let Some(key) = key && !script::pure::built_nothing(job, unit) {
        let record = RecordRef {
            ok: compiled.ok,
            diagnostics: &compiled.diagnostics,
            outputs: compiled.outputs.iter().map(|it| (it.path.as_str(), it.key, it.executable)).collect(),
            artifact: compiled.artifact.as_ref(),
            made: &made,
        };
        let keys: Vec<Key> = compiled.outputs.iter().map(|it| it.key).collect();
        let mut bytes = Vec::new();
        header::write(&mut bytes, &keys, &natives);
        blaze_protocol::cbor::append(&mut bytes, &record)
            .map_err(|why| ApiError::new(format!("encoding the record of unit {unit}: {why}"), None))?;
        let record = Put { key, kind: Kind::Query, bytes };
        if shared(job, unit) {
            compiled.share = share(job, unit, &compiled, &record)?;
        }
        compiled.puts.push(record);
    }
    if let Some(slot) = job.made.get(unit) {
        let _ = slot.set(made);
    }
    Ok(compiled)
}

/// The expanders of proc-macro unit `unit`, for the crates that use it
/// ([`crate::serve::work::Writer::macros`]); none for a unit that is no proc macro or did not
/// build. Each runs the macro's own code on blaze's compiler in this process
/// ([`blaze_macro_eval::load`], docs/v2/plan/design/compile-time.md change 20), isolated by
/// construction: nothing it compiled to is loaded, and no process is started. Asked again each
/// time the unit compiles or its record is reused, as the front end it reads may have changed.
fn macros(job: &Job, reader: &UnitReader, unit: usize, ok: bool) -> Option<ProcMacroLoadResult> {
    if !ok || !job.crates.is_proc_macro(unit) {
        return None;
    }
    let name = job.unit(unit).map_or("", |it| it.target.name.as_str());
    let failed = |what: String| Some(Err(ProcMacroLoadingError::Refused(what.into())));
    let Some(krate) = job.crates.krate(unit) else {
        return failed(format!("proc macro `{name}` has no crate in the front end"));
    };
    let toolchain = match codegen::toolchain(job, unit) {
        Ok(it) => it.id,
        Err(why) => return failed(format!("proc macro `{name}`: its toolchain: {why}")),
    };
    Some(blaze_macro_eval::load(reader.db(), krate, toolchain))
}

/// The lint flags of unit `unit`'s check, as its check is given them.
pub(crate) fn unit_lint_flags(job: &Job, unit: usize) -> Result<Vec<String>, ApiError> {
    let u = job.unit(unit)?;
    let package = job.package(u)?;
    Ok(lint_flags(u, job.plan(unit)?, local(package.source.as_deref()), job.input.verbose > 1))
}

pub(crate) use blaze_lower::{lint_flags, local};

/// Whether a diagnostic is one of rustc's closing lines cargo does not show: "aborting due to
/// ...", "N warnings emitted" (`cargo/src/cargo/core/compiler/mod.rs`, `on_stderr_line_inner`).
pub(crate) fn cargo_hides(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with("warning emitted")
        || message.ends_with("warnings emitted")
}



/// `unit`'s crate in the front end.
fn crate_of(job: &Job, unit: usize, u: &Unit) -> Result<base_db::Crate, ApiError> {
    job.crates.krate(unit).ok_or_else(|| {
        // alloc: error path
        ApiError::new(format!("unit {unit} (`{}`) has no crate in the front end", u.target.name), None)
    })
}

/// The lint flags of `unit`'s check, and the options they give it: an error for flags that
/// name no lint blaze has, whether or not a check runs.
fn options(job: &Job, unit: usize, u: &Unit) -> Result<(Vec<String>, Options), ApiError> {
    let package = job.package(u)?;
    let flags = lint_flags(u, job.plan(unit)?, local(package.source.as_deref()), job.input.verbose > 1);
    let mut lints = CommandLine::parse(&flags)
        .map_err(|why| ApiError::new(why, Some(format!("the lint flags of `{}`", u.target.name))))?;
    // Every crate blaze builds is built as cargo builds it, with `CARGO_CRATE_NAME` set.
    lints.check_cfg.cargo = true;
    Ok((flags, Options { lints, ..Options::default() }))
}

/// [`Phase::Link`]'s making of the unit, from what its phases before left in `phases`, which
/// it only reads: its diagnostics as cargo shows them, then, unless they fail it, the files
/// rustc would write, each image linked (relinking `images`).
fn assemble(job: &Job, reader: &UnitReader, unit: usize, phases: &Phases, images: &mut Vec<Image>) -> Result<Making, Failed> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let unit_of = job.unit_of(unit)?;
    let color = job.input.color;
    let mounted = &job.mounted;
    let (Some(Does { inert, .. }), Some(diags)) = (phases.does, &phases.diags) else {
        // alloc: error path
        let what = format!("unit {unit} (`{}`) is linked before its names are read and its check is done", u.target.name);
        return Err(ApiError::new(what, None).into());
    };
    let mut sources = blaze_diagnostics::emit::Sources::default();
    for (name, text) in &phases.unmounted {
        // clone: a file no mount holds, once per unit whose diagnostics point into one
        sources.add(name, Some(text.clone()));
    }
    mounted.add_sources(&mut sources, diags);
    let mut style = blaze_diagnostics::emit::Style { ansi: color, short: job.input.short, ..Default::default() };
    if let Some(width) = job.input.width {
        style.width = usize::try_from(width).unwrap_or(usize::MAX);
    }
    let mut diagnostics: Vec<Diagnostic> = diags
        .iter()
        .filter(|it| !cargo_hides(&it.message))
        .map(|it| blaze_diagnostics::api::diagnostic(it, &sources, style, Some(unit_of.clone())))
        .collect::<Result<_, _>>()
        // clone: the unit's name, on the error path
        .map_err(|why| ApiError::new(why.to_string(), Some(u.target.name.clone())))?;
    let failed = diagnostics.iter().any(|it| it.level == Level::Error);
    let mut made = Made::default();
    let nothing = |diagnostics: Vec<Diagnostic>, ok: bool, made: Made| Making {
        diagnostics,
        ok,
        made,
        nothing: true,
        files: Vec::new(),
        puts: Vec::new(),
        natives: Vec::new(),
        links: Vec::new(),
        macros: None,
    };
    if failed {
        return Ok(nothing(diagnostics, false, made));
    }
    if job.cancelled() {
        return Err(Failed::Again);
    }
    let mut natives = Vec::new();
    let mut links = Vec::new(); // alloc: none until a link
    // What each file rustc would write holds: (path, bytes, executable).
    let mut files: Vec<(String, Vec<u8>, bool)> = Vec::new();
    let identity = identity(job, unit);
    if inert {
        // A build script that does nothing is not run, so not built: what it prints is known.
        return Ok(nothing(diagnostics, true, made));
    }
    match &phases.generated {
        None => {
            for path in artifact::made(plan) {
                files.push((path.to_owned(), crate_record(u, identity, &[]), false));
            }
        }
        Some(Err(what)) => {
            // clone: error path
            diagnostics.push(say::refused(what.clone(), Some(unit_of), color));
            return Ok(nothing(diagnostics, false, made));
        }
        Some(Ok((makes, generated))) => {
            let parts = match generated.as_ref().map_err(|what| ApiError::new(what.clone(), Some(u.target.name.clone())))? {
                Ok(it) => it,
                Err(errors) => {
                    // clone: error path
                    let what = crate::refused_error(&u.target.name, errors.len(), errors.iter().cloned(), job.input.refused);
                    diagnostics.push(say::refused(what, Some(unit_of), color));
                    return Ok(nothing(diagnostics, false, made));
                }
            };
            if job.cancelled() {
                return Err(Failed::Again);
            }
            let generated = parts.over(job, reader, mounted);
            made.context = Some(context(generated.policy));
            made.inputs = generated.built.inputs.iter().map(|(key, code)| (key.0, code.0)).collect();
            // A test's output prints positions (a panic's `file:line:col`), which no code hash
            // has: a reformat that moves a `panic!` changes its message. Every span table of the
            // binary is in each test's selection, so such an edit runs its tests again.
            // alloc: once per test unit, one hash per span table
            let spans: Vec<CodeHash> = match generated.roots.tests.is_empty() {
                true => Vec::new(),
                false => generated
                    .side()
                    .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?
                    .spans
                    .iter()
                    .map(|(anchor, table)| {
                        let mut hasher = Hasher::new(SPANS);
                        hasher.id(anchor).part(table);
                        CodeHash(hasher.finish())
                    })
                    .collect(),
            };
            for test in &generated.roots.tests {
                let root = generated.built.roots.get(test.root).ok_or_else(|| {
                    ApiError::new(
                        format!("test `{}` has no root among the build's", test.name),
                        Some(u.target.name.clone()),
                    )
                })?;
                let mut reached = generated.built.reached(root);
                reached.extend_from_slice(&spans);
                reached.sort_unstable();
                reached.dedup();
                made.tests.push(TestOf {
                    name: test.name.clone(),
                    ignore: test.ignore,
                    should_panic: test.should_panic.clone(),
                    bench: test.bench,
                    outside: test.outside,
                    reached,
                });
            }
            let roots: Vec<ContentId> = generated.built.roots.iter().map(|it| it.0).collect();
            for path in artifact::made(plan) {
                let library = path.ends_with(".rlib") || path.ends_with(".rmeta");
                if library || *makes == Makes::Library {
                    files.push((path.to_owned(), crate_record(u, identity, &roots), false));
                    continue;
                }
                match codegen::link(job, unit, path, makes, &generated, images) {
                    Ok(linked) => {
                        natives.extend(linked.read);
                        if linked.table.is_some() {
                            made.table = linked.table;
                        }
                        // alloc: once per image linked, a unit links one or two
                        links.push((path.to_owned(), linked.how));
                        files.push((path.to_owned(), linked.bytes, true));
                    }
                    Err(failed) => {
                        // alloc: error path
                        diagnostics.push(say::refused(failed.to_string(), Some(unit_of), color));
                        return Ok(nothing(diagnostics, false, made));
                    }
                }
            }
        }
    }
    // alloc: none: the built puts are moved in after, and the outputs by the store
    Ok(Making { diagnostics, ok: true, made, nothing: false, files, puts: Vec::new(), natives, links, macros: None })
}

/// A test binary's `context` with the content of each native input its link read, in link
/// order; `context` itself when it read none.
pub(crate) fn with_natives(context: ContentId, natives: &[(String, ContentId)]) -> ContentId {
    if natives.is_empty() {
        return context;
    }
    let mut hasher = Hasher::new(NATIVES);
    hasher.id(&context);
    for (_, native) in natives {
        hasher.id(native);
    }
    hasher.finish()
}

/// How `unit`'s program is run: its plan's runner, and cargo's variables of it, from the build
/// scripts' runs it depends on (its package's script's `rustc-env`, every run's library search
/// directories). Worked out once per job and unit: the record's key and the artifact read it.
fn runs(job: &Job, unit: usize) -> artifact::Runs {
    if let Some(slot) = job.runs.get(unit) {
        return slot.get_or_init(|| work_out_runs(job, unit)).clone();
    }
    artifact::Runs::default()
}

fn work_out_runs(job: &Job, unit: usize) -> artifact::Runs {
    let units = job.units();
    let Some(plan) = units.plans.get(unit) else { return artifact::Runs::default() };
    let package = job.graph.package(unit as u32);
    let mut own: &[(String, String)] = &[];
    let mut native: Vec<String> = Vec::new();
    let mut seen = blaze_hash::HashSet::default();
    for at in job.graph.closure(unit as u32) {
        if !job.graph.has(at, graph::flag::RUN) {
            continue;
        }
        let Some(applied) = job.scripts[at as usize].get() else { continue };
        if job.graph.package(at) == package {
            own = &applied.output.env;
        }
        for path in &applied.output.library_paths {
            let path = blaze_build_script::output::search_path_of(path.path());
            if seen.insert(path) {
                native.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let cargo = job.shared.launcher.to_string_lossy();
    artifact::Runs {
        runner: plan.runner.clone(),
        env: artifact::run_env(plan, &cargo, own, &native, std::env::consts::OS),
    }
}

/// What a test binary's behaviour depends on beside the code it reaches.
fn context(policy: &blaze_codegen::plan::Policy) -> ContentId {
    let bytes = blaze_protocol::cbor::to_vec(policy).unwrap_or_default();
    ContentId::of("blaze v2 test context 2", &bytes)
}

/// What blaze writes for a library (`.rlib`, `.rmeta`): no object code (its functions are in
/// the store, by key, for whatever links it), but an `ar` archive, as rustc's are, with one
/// member `lib.rmeta` saying which unit it is, what it was made from, and its roots' keys.
pub(crate) fn crate_record(unit: &Unit, identity: Option<ContentId>, roots: &[ContentId]) -> Vec<u8> {
    let mut member = b"blaze v2 crate 1\n".to_vec();
    member.extend_from_slice(format!("{} {}\n", unit.pkg_id, unit.target.name).as_bytes());
    if let Some(identity) = identity {
        member.extend_from_slice(&identity.0);
    }
    member.extend_from_slice(&(roots.len() as u64).to_le_bytes());
    for root in roots {
        member.extend_from_slice(&root.0);
    }
    ar(&[("lib.rmeta", &member)])
}

/// A System V `ar` archive of `members`, deterministic (no time, no owner, mode 644).
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        let header = format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{name}/"), 0, 0, 0, 644, bytes.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The files of `krate`'s modules, by absolute path, sorted.
fn crate_files(reader: &UnitReader, krate: base_db::Crate) -> Vec<String> {
    let db = reader.db();
    let paths = reader.paths();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = Vec::new();
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id()
            && let Some(path) = paths.get(&file.file_id(db))
        {
            out.push(path);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.into_iter().cloned().collect()
}

/// A dep-info file as rustc (`deps/foo-0123.d`: its outputs, then each source by the name
/// rustc gives it) or cargo (`target/debug/foo.d`: the uplifted file and absolute sources)
/// writes it.
pub(crate) fn dep_info(path: &str, plan: &blaze_units::query::UnitPlan, main: Option<&str>, sources: &[String], mounted: &Mounted) -> String {
    let escape = |it: &str| it.replace(' ', "\\ ");
    let rustc = plan.extra_filename.is_empty() || path.contains(plan.extra_filename.as_str());
    match rustc {
        true => {
            let names: Vec<String> = sources.iter().map(|it| escape(&mounted.name(it))).collect();
            let deps = names.join(" ");
            let mut out = String::new();
            for target in main.into_iter().chain(std::iter::once(path)) {
                out += &format!("{}: {deps}\n\n", escape(target));
            }
            for name in &names {
                out += &format!("{name}:\n");
            }
            out
        }
        false => {
            let target = main.and_then(|it| artifact::uplifted(it, plan)).or(main).unwrap_or(path);
            let deps: Vec<String> = sources.iter().map(|it| escape(it)).collect();
            format!("{}: {}\n", escape(target), deps.join(" "))
        }
    }
}
