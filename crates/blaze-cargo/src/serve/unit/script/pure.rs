//! Pure build scripts (docs/v2/plan/lanes/pure-scripts/design.md): a script `hir_ty_pure`
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
    run::Value,
};

use super::{Of, compile_of, inherited, named_program, script_unit};
use crate::serve::unit::{Job, UnitReader, error::UnitError};

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
            format!("{PURE}={value} is not a value blaze takes: `verify` builds and runs a pure build script whose result was kept as well, unset runs none"),
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
    /// Each run of the script, by its unit, with the kept value its pure key found, if any.
    pub(crate) runs: Vec<(usize, Option<Value>)>,
}

impl Pure {
    fn hit(&self, run: usize) -> Option<&Value> {
        self.runs.iter().find(|it| it.0 == run).and_then(|it| it.1.as_ref())
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
    let verify = verifies(job)?;
    let Some(slot) = job.pure.get(unit) else { return Ok(false) };
    if slot.get().is_none() {
        let worked = work_out(job, reader, unit)?;
        let _ = slot.set(worked);
    }
    Ok(!verify && of(job, unit).is_some_and(Pure::all_hit))
}

/// Whether compile `unit` built nothing because its runs hit ([`unbuilt`], already asked): then
/// it keeps no record.
pub(crate) fn built_nothing(job: &Job, unit: usize) -> bool {
    !verifies(job).unwrap_or(true) && of(job, unit).is_some_and(Pure::all_hit)
}

fn work_out(job: &Job, reader: &UnitReader, compile: usize) -> Result<Purity, ApiError> {
    let db = reader.db();
    let krate = job.crates.krate(compile).ok_or(UnitError::NoCrate { unit: compile })?;
    // Not proved, so built and run in the sandbox as any script: the proof is asked of a free
    // `main` only.
    let Some(main) = hir_ty_pure::main_of(db, krate) else { return Ok(Purity::Unproved) };
    let purity = hir_ty::attach_db(db, || hir_ty_pure::script_purity(db, main));
    let proved = match purity {
        hir_ty_pure::Purity::Pure(proved) => proved,
        // alloc: once per impure script: what the build's report says of it
        hir_ty_pure::Purity::Impure(impure) => return Ok(Purity::Impure(impure.said())),
    };
    let proof = Proof {
        identity: proved.identity,
        // alloc: once per pure script: the names its key reads
        env: proved.env.iter().map(|it| OsString::from(it.as_str())).collect(),
        reads: proved.effects.has(hir_ty_pure::Effects::READ) || proved.effects.has(hir_ty_pure::Effects::PROBE),
        writes: proved.effects.has(hir_ty_pure::Effects::WRITE),
        prints: proved.effects.has(hir_ty_pure::Effects::PRINT),
    };
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
        let found = pure::find(&job.input.store, &guard, &proof, &mut disk)
            .map_err(|what| ApiError::new(what, Some(unit.package.descr())))?;
        runs.push((run, found));
    }
    Ok(Purity::Pure(Pure { proof, runs }))
}

/// Run unit `run` as `blaze_build_script` describes it, and the environment it is started in:
/// what its pure key's variables are read from. Its program is named, not built.
fn run_of(job: &Job, run: usize) -> Result<(bs::Unit, Vec<(OsString, OsString)>), ApiError> {
    let units = job.units();
    let compile = compile_of(units, run)
        // alloc: the edge: the error the build stops with
        .ok_or_else(|| ApiError::new(format!("the run of unit {run} has no build script compiled"), None))?;
    let of = Of {
        units,
        host: &job.input.config.host,
        tools: &job.shared.tools,
        launcher: &job.shared.launcher,
        program: named_program(units, compile)?,
        scripts: &job.scripts,
    };
    // alloc: the edge: the error the build stops with
    let unit = script_unit(&of, run).map_err(|what| ApiError::new(what, Some(format!("the build script of unit {run}"))))?;
    let env = inherited(&job.input.config.invocation.env, job.plan(run)?);
    Ok((unit, env))
}

/// The environment `unit`'s script sees, started in `inherited`: what a pure key's variables
/// have the values of, as [`blaze_usercode::script::prepare`] gives a run its own.
fn script_env(unit: &bs::Unit, inherited: &[(OsString, OsString)]) -> Result<Vec<(OsString, OsString)>, ApiError> {
    blaze_build_script::env(unit, inherited)
        .map(|it| it.vars)
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
pub(crate) fn check(job: &Job, run: usize, unit: &bs::Unit, traced: &Traced) -> Option<Result<Read, String>> {
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

/// Keeps `value`, the result of run `run` of `unit`'s pure script, under its pure key, after
/// checking what it printed and wrote against its proof; `checked` is what [`check`] said of
/// its trace. A run that failed is not kept, as no failed run is; a trace that showed an effect
/// the proof said absent is an error whether it failed or not.
pub(crate) fn keep(
    job: &Job,
    run: usize,
    unit: &bs::Unit,
    checked: Option<Result<Read, String>>,
    store: &mut Store,
    disk: &mut Disk,
    value: &Value,
) -> Result<(), String> {
    let Some(checked) = checked else { return Ok(()) };
    let read = checked?;
    if !value.succeeded() {
        return Ok(());
    }
    let Some(pure) = compile_of(job.units(), run).and_then(|compile| of(job, compile)) else { return Ok(()) };
    let script = unit.package.descr();
    pure::checked_value(&script, &pure.proof, value).map_err(|it| it.0)?;
    pure::keep(store, &pure.proof, read, disk, value)
        .map(|_| ())
        // alloc: the edge: the error the build stops with
        .map_err(|error| format!("the build script of `{script}`: keeping its result under its pure key: {error:?}"))
}
