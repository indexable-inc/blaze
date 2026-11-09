//! Pure build scripts (docs/v2/plan.md): a script `hir_ty_pure`
//! proves pure from its MIR is looked up under its pure key ([`blaze_usercode::pure`]) as its
//! compile begins, on the worker that has the front end. When every run of it hits, it is
//! neither built nor run: each run's value is the kept one, its `OUT_DIR` written back. When a
//! run misses, the script is built and run in the sandbox as any other, its trace checked
//! against the proof ([`check`]) and its result kept under the pure key as well ([`keep`]).
//!
//! `BLAZE_PURE=verify` builds and runs a script that hit as well, and fails the build where its
//! run differs from the kept value (the run is checked as an elided one is,
//! [`blaze_usercode::script::verify_elided`]).
//!
//! A compile that built nothing because its runs hit keeps no record: its record would say the
//! script has no program, and a later build whose pure key misses (a package file changed, which
//! the compile's own key does not read) needs one.

#![forbid(unsafe_code)]

use std::{ffi::OsString, path::Path};

use blaze_api::ApiError;
use blaze_build_script::unit as bs;
use blaze_cas::Store;
use blaze_core::Traced;
use blaze_units::schema::Mode;
use blaze_usercode::{
    inputs::Disk,
    pure::{self, Places, Proof, Read},
    roots::RootMap,
    run::Value,
};

use super::{Of, compile_of, inherited, named_program, outside, run_roots, script_unit};
use crate::serve::{
    unit::{Job, UnitReader, error::UnitError},
    work::RunPuts,
};

/// The job's variable that asks a script whose pure key hit built and run as well, and checked
/// against the kept value: `verify`, or unset.
pub(crate) const PURE: &str = "BLAZE_PURE";

/// Whether the job asks `BLAZE_PURE=verify`; any other value of it is an error naming it.
pub(crate) fn verifies(job: &Job) -> Result<bool, ApiError> {
    match job.input.config.invocation.env.iter().find(|(name, _)| name == PURE) {
        None => Ok(false),
        Some((_, value)) if value == "verify" => Ok(true),
        Some((_, value)) => Err(ApiError::new(
            // alloc: the edge: the error the build stops with
            format!(
                "{PURE}={value} is not a value blaze takes: `verify` builds and runs a pure build script whose result was kept as well, unset runs none"
            ),
            None,
        )),
    }
}

/// What the pure path found of one build script, by its compile unit: what a build's report
/// says of each script.
#[derive(Debug)]
pub(crate) enum Purity {
    /// Its crate has no free `main` with a body (`hir_ty_pure::main_of`): nothing was asked.
    Unproved,
    /// Not proved pure: the first effect found, and the calls that reach it
    /// ([`hir_ty_pure::Impure::said`]). It runs in the sandbox, as any script.
    Impure(String),
    Pure(Pure),
}

/// A build script proved pure, and its runs' lookups under its pure key.
#[derive(Debug)]
pub(crate) struct Pure {
    pub(crate) proof: Proof,
    /// Each run of the script, by its unit, with what its pure key found, if anything: the
    /// kept value, and whether it was verified across checkouts.
    pub(crate) runs: Vec<(usize, Option<pure::Hit>)>,
}

impl Pure {
    fn found(&self, run: usize) -> Option<&pure::Hit> {
        self.runs.iter().find(|it| it.0 == run).and_then(|it| it.1.as_ref())
    }

    fn hit(&self, run: usize) -> Option<&Value> {
        self.found(run).map(|it| &it.value)
    }

    fn all_hit(&self) -> bool {
        !self.runs.is_empty() && self.runs.iter().all(|it| it.1.is_some())
    }
}

/// What the pure path found of the build script compiled by unit `compile`, worked out once on
/// the worker that compiles it ([`unbuilt`]); `None` for a unit that is no build script's
/// compile, one with the sandbox off, or one not worked out yet (a run reads it after its
/// compile).
pub(crate) fn purity(job: &Job, compile: usize) -> Option<&Purity> {
    job.pure.get(compile)?.get()
}

/// [`purity`], when the script is pure.
pub(crate) fn of(job: &Job, compile: usize) -> Option<&Pure> {
    match purity(job, compile)? {
        Purity::Pure(pure) => Some(pure),
        Purity::Unproved | Purity::Impure(_) => None,
    }
}

/// Whether build script compile `unit` builds nothing: the script is pure and every run of it
/// found its value under its pure key. Works the proof and the lookups out the first time it is
/// asked, reading the front end through `reader`.
pub(crate) fn unbuilt(job: &Job, reader: &UnitReader, unit: usize) -> Result<bool, ApiError> {
    let u = job.unit(unit)?;
    if !u.target.is_custom_build() || matches!(u.mode, Mode::RunCustomBuild) || job.sandbox_off() {
        return Ok(false);
    }
    // `BLAZE_SCRIPT_CACHE=check` reruns what the store kept, a pure key's hit among it.
    let verify = verifies(job)? || super::rechecks(job)?;
    let Some(slot) = job.pure.get(unit) else { return Ok(false) };
    if slot.get().is_none() {
        // Named for the script it proves (`serde v1.0.219 (build script)`): every build
        // script's target is `build-script-build`, so the crate alone can't tell two apart.
        // alloc: once per build script, the name its span carries
        let name = crate::serve::unit::compile::span_unit(u);
        let krate = u.target.name.as_str();
        let _span =
            tracing::info_span!(target: "blaze", "build script purity", krate, unit = name.as_str()).entered();
        let worked = work_out(job, reader, unit, krate, &name)?;
        let _ = slot.set(worked);
    }
    Ok(!verify && of(job, unit).is_some_and(Pure::all_hit))
}

/// Whether compile `unit` built nothing because its runs hit ([`unbuilt`], already asked): then
/// it keeps no record.
pub(crate) fn built_nothing(job: &Job, unit: usize) -> bool {
    !verifies(job).unwrap_or(true)
        && !super::rechecks(job).unwrap_or(true)
        && of(job, unit).is_some_and(Pure::all_hit)
}

/// The most instances a build script's purity proof may walk before it stops with an error
/// naming the script: the bound on the walk (rule 18). A step is a body instance the walk
/// reaches, each once ([`hir_ty_pure::ScriptProof::steps`]), whatever the daemon had cached
/// and whichever task ran what, so a script is proved or refused alike on a cold, warm or busy
/// daemon. How many a proof took is the `steps` of its `build script proof` span: the margin
/// under this bound is read from the profiles. No real proof's count is quoted here yet;
/// calibrate it from the largest (cranelift-codegen's and wasmtime's among them).
const PROOF_STEPS: u64 = 1 << 20;

/// The most rows of the front end a purity proof may bring up to date (each run or verified,
/// each time it is reached): a bound on the query engine under the proof, not on the script,
/// so an engine that would not end the proof stops with an error that says so (rule 18).
/// Unlike [`PROOF_STEPS`] it counts what was not cached, so it sits far above any proof's
/// real work: a verify checks each row of a cycle once (`blaze_query`'s pending rows), and a
/// cold proof of cranelift-codegen's build script brought fewer than 2^25 up to date.
const ENGINE_FUEL: u64 = 1 << 28;

/// What the pure path finds of the script compiled by unit `compile`, its target `target`
/// (`build-script-build`) and `unit` its name (`serde v1.0.219 (build script)`), which the
/// proof's span carries.
fn work_out(
    job: &Job,
    reader: &UnitReader,
    compile: usize,
    target: &str,
    unit: &str,
) -> Result<Purity, ApiError> {
    let db = reader.db();
    let krate = job.crates.krate(compile).ok_or(UnitError::NoCrate { unit: compile })?;
    // Not proved, so built and run in the sandbox as any script: the proof is asked of a free
    // `main` only.
    let Some(main) = hir_ty_pure::main_of(db, krate) else { return Ok(Purity::Unproved) };
    let proof = {
        // The walk from `main` over every instance it reaches: the part of the span above
        // that is not the store's lookups below.
        let span = blaze_profile::span::ScriptProof {
            krate: blaze_profile::span::Name::of(target),
            unit: blaze_profile::span::Name::of(unit),
        }
        .open();
        let proved = blaze_query::db::with_fuel(ENGINE_FUEL, || {
            blaze_query::db::with_steps(PROOF_STEPS, || {
                blaze_query::db::attach(db, || hir_ty_pure::script_proof(db, main))
            })
        });
        match proved {
            // A proof kept from a walk with no bound on it is held to the bound as well.
            Ok((Ok((proof, _)), _)) if u64::from(proof.steps) <= PROOF_STEPS => {
                span.ended(blaze_profile::span::ScriptProved { steps: u64::from(proof.steps) });
                proof
            }
            // alloc: an error, once
            Ok((Ok((proof, _)), _)) => {
                return Err(refused(job, compile, &format!("it walked {}", proof.steps)));
            }
            Ok((Err(out), _)) => return Err(refused(job, compile, &format!("it {out}"))), // alloc: an error, once
            Err(out) => {
                let what = format!(
                    "did not end: the query engine under it {out}, past the {ENGINE_FUEL} rows a proof may bring up to \
                     date (rule 18). The walk itself is bounded by the instances it reaches, so this is the engine \
                     not ending, a blaze bug, not the script"
                ); // alloc: an error, once
                return Err(proof_error(job, compile, &what));
            }
        }
    };
    let proved = match &proof.purity {
        hir_ty_pure::Purity::Pure(proved) => proved,
        // alloc: once per impure script: what the build's report says of it
        hir_ty_pure::Purity::Impure(impure) => return Ok(Purity::Impure(impure.said())),
    };
    let proof = Proof {
        identity: proved.identity,
        // alloc: once per pure script: the names its key reads
        env: proved.env.iter().map(|it| OsString::from(it.as_str())).collect(),
        reads: proved.effects.has(hir_ty_pure::Effects::READ)
            || proved.effects.has(hir_ty_pure::Effects::PROBE),
        writes: proved.effects.has(hir_ty_pure::Effects::WRITE),
        prints: proved.effects.has(hir_ty_pure::Effects::PRINT),
    };
    let _span = tracing::info_span!(target: "blaze", "build script pure lookup").entered();
    let units = job.units();
    let mut runs = Vec::new(); // alloc: once per pure script, one entry per run of it
    for run in 0..units.graph.units.len() {
        let is_run = matches!(units.graph.units[run].mode, Mode::RunCustomBuild);
        if !is_run || compile_of(units, run) != Some(compile) {
            continue;
        }
        let (unit, env) = run_of(job, run)?;
        let mut disk = Disk::new(script_env(&unit, &env)?);
        let guard = blaze_cas::enter();
        let roots = run_roots(job, &unit);
        let found = pure::find_hit(&job.input.store, &guard, &proof, roots.view(), &mut disk)
            .map_err(|what| ApiError::new(what, Some(unit.package.descr())))?;
        runs.push((run, found));
    }
    Ok(Purity::Pure(Pure { proof, runs }))
}

/// The error of the purity proof of the script compiled by unit `compile`, which walked more
/// instances than [`PROOF_STEPS`]: `how` says how far it got.
fn refused(job: &Job, compile: usize, how: &str) -> ApiError {
    // alloc: an error, once
    let what = format!(
        "walks more than the {PROOF_STEPS} instances a proof's walk may reach (rule 18): {how}"
    );
    proof_error(job, compile, &what)
}

/// The error of the purity proof of the script compiled by unit `compile`, naming the script;
/// `what` says what the proof did.
fn proof_error(job: &Job, compile: usize, what: &str) -> ApiError {
    let unit = job.unit(compile);
    let script = unit
        .as_ref()
        .map_or_else(|it| it.what.clone(), |it| format!("`{}` of `{}`", it.target.name, it.pkg_id)); // alloc: an error, once
    let what = format!(
        "the purity proof of the build script {script} (`script_proof` of its `main`) {what}"
    ); // alloc: an error, once
    ApiError::new(what, unit.ok().map(|it| it.pkg_id.clone())) // clone: an error, once
}

/// Run unit `run` as `blaze_build_script` describes it, and the environment it is started in:
/// what its pure key's variables are read from. Its program is named, not built.
fn run_of(job: &Job, run: usize) -> Result<(bs::Unit, Vec<(OsString, OsString)>), ApiError> {
    let units = job.units();
    let compile = compile_of(units, run)
        // alloc: the edge: the error the build stops with
        .ok_or_else(|| {
            ApiError::new(format!("the run of unit {run} has no build script compiled"), None)
        })?;
    let of = Of {
        units,
        host: &job.input.config.host,
        tools: &job.shared.tools,
        launcher: &job.shared.launcher,
        program: named_program(units, compile)?,
        scripts: &job.scripts,
        native: &job.input.native,
    };
    // alloc: the edge: the error the build stops with
    let unit = script_unit(&of, run)
        .map_err(|what| ApiError::new(what, Some(format!("the build script of unit {run}"))))?;
    // As `prepare` starts the run in it, so the pure key reads what the run is given.
    let env = inherited(&job.input.config.invocation.env, job.plan(run)?, !outside(job, run));
    Ok((unit, env))
}

/// The environment `unit`'s script sees, started in `inherited`: what a pure key's variables
/// have the values of, the whole of it as [`blaze_usercode::script::prepare`] starts a run in
/// it ([`blaze_usercode::script::Prepared::script_env`], what [`keep`] reads them from).
fn script_env(
    unit: &bs::Unit,
    inherited: &[(OsString, OsString)],
) -> Result<Vec<(OsString, OsString)>, ApiError> {
    blaze_usercode::script::script_env(unit, inherited)
        .map_err(|error| ApiError::new(error.0, Some(unit.package.descr())))
}

/// What a build's report says of run unit `run`'s script: [`purity`] of its compile, and
/// whether the run's pure key found its value (it was not run).
pub(crate) fn reported(job: &Job, run: usize) -> Option<(&Purity, bool)> {
    let purity = purity(job, compile_of(job.units(), run)?)?;
    Some((purity, hit(job, run).is_some()))
}

/// The value run unit `run`'s pure key found, when its script is pure and it hit.
pub(crate) fn hit(job: &Job, run: usize) -> Option<&Value> {
    of(job, compile_of(job.units(), run)?)?.hit(run)
}

/// What `traced`, the whole trace of run `run` of `unit`'s script, shows against its proof,
/// when the script is pure: the package files it read, or the error naming the effect its
/// proof said it never has. `None` for a script that is not pure. Asked before the run's own
/// key takes anything out of the trace.
pub(crate) fn check(
    job: &Job,
    run: usize,
    unit: &bs::Unit,
    traced: &Traced,
) -> Option<Result<Read, String>> {
    let pure = of(job, compile_of(job.units(), run)?)?;
    // clone: once per run of a pure script: the two paths its loader reads may come from
    let toolchain = [job.shared.tools.clone(), job.shared.launcher.clone()];
    let places = Places {
        package: &unit.package.manifest_dir,
        out_dir: &unit.out_dir,
        target_dir: Path::new(&job.units().target_dir),
        program: &unit.script,
        toolchain: &toolchain,
    };
    Some(pure::check(&unit.package.descr(), &pure.proof, &places, traced).map_err(|it| it.0))
}

/// Run `run` of a pure script whose pure key found a value verified across checkouts, run
/// again (`BLAZE_SCRIPT_CACHE=check`, `BLAZE_PURE=verify`), against that value
/// ([`pure::retract`]): when it gave otherwise, the record that the value depends on where its
/// roots are is put, and how it differs is given with the record's ticket, durable before the
/// run is reported done. None when its script is not pure, its pure key missed, the hit was
/// this checkout's own, or the run gave the same. An error carries what landed before it.
pub(crate) fn retract(
    job: &Job,
    run: usize,
    unit: &bs::Unit,
    store: &mut Store,
    roots: &RootMap,
    value: &Value,
) -> Result<Option<(String, RunPuts)>, (String, RunPuts)> {
    let Some(pure) = compile_of(job.units(), run).and_then(|compile| of(job, compile)) else {
        return Ok(None);
    };
    let Some(hit) = pure.found(run) else { return Ok(None) };
    match pure::retract(store, &pure.proof, roots.view(), hit, value) {
        Ok(None) => Ok(None),
        Ok(Some((what, tickets))) => {
            // Every put of the record, each segment's last: one ticket alone bounds only its own.
            let mut puts = RunPuts { retracted: true, ..RunPuts::default() };
            puts.kept(tickets);
            Ok(Some((what, puts)))
        }
        Err(not) => {
            // alloc: the edge: the error the build stops with
            let what = format!(
                "the build script of `{}`: retracting its pure key's verified result: {:?}",
                unit.package.descr(),
                not.error
            );
            Err((what, landed(not)))
        }
    }
}

/// Keeps `value`, the result of run `run` of `unit`'s pure script, under its pure key, after
/// checking what it printed and wrote against its proof; `checked` is what [`check`] said of
/// its trace. A run that failed is not kept, as no failed run is; a trace that showed an effect
/// the proof said absent is an error whether it failed or not. `roots` are the run's prepared
/// ones (its spec's), never made again here: a root's links are compared with how they were
/// as it was prepared ([`blaze_usercode::roots::Roots::relinked`]). `script_env` is the
/// script's whole environment ([`blaze_usercode::script::Prepared::script_env`]): a pure key has
/// no given part, so its variables are kept by their values from all of it, as [`work_out`]'s
/// lookup reads them ([`script_env`]), never from the run's own check, which leaves the given
/// names out. The ticket of a record that
/// the run depends on where its roots are, when one was put: durable before the run is
/// reported done. An error carries what landed before it.
pub(crate) fn keep(
    job: &Job,
    run: usize,
    unit: &bs::Unit,
    checked: Option<Result<Read, String>>,
    store: &mut Store,
    script_env: &[(OsString, OsString)],
    roots: &RootMap,
    value: &Value,
) -> Result<RunPuts, (String, RunPuts)> {
    let Some(checked) = checked else { return Ok(RunPuts::default()) };
    let read = checked.map_err(|what| (what, RunPuts::default()))?;
    if !value.succeeded() {
        return Ok(RunPuts::default());
    }
    let Some(pure) = compile_of(job.units(), run).and_then(|compile| of(job, compile)) else {
        return Ok(RunPuts::default());
    };
    let script = unit.package.descr();
    pure::checked_value(&script, &pure.proof, value).map_err(|it| (it.0, RunPuts::default()))?;
    let mut disk = Disk::new(script_env.to_vec()); // alloc: once per kept run of a pure script, its key's environment
    pure::keep(store, &pure.proof, roots.view(), read, &mut disk, value)
        .map(|kept| {
            let mut puts = RunPuts::default();
            puts.kept(kept.tickets);
            puts.retraction(kept.retracted);
            puts
        })
        .map_err(|not| {
            // alloc: the edge: the error the build stops with
            let what = format!(
                "the build script of `{script}`: keeping its result under its pure key: {:?}",
                not.error
            );
            (what, landed(not))
        })
}

/// What a keep that failed part way put before it did: its tickets, waited for as any put's.
fn landed(not: blaze_usercode::roots::verify::NotKept) -> RunPuts {
    let mut puts = RunPuts::default();
    puts.kept(not.tickets);
    puts.retraction(not.retracted);
    puts
}
