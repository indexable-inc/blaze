//! blaze: the lints over every configuration of a package that `blaze check` linted
//! (`blaze::dead`, `blaze::test_only`, `blaze::cfg_only`; [`blaze_diagnostics::reach`],
//! docs/v2/plan/design/multiconfig-check.md), given once every unit of the job ends.
//!
//! A configuration is one linted unit: the lib, the lib checked as a test, each bin, test,
//! bench and example, as this job built them (the host and the features asked for; more
//! platforms and feature sets are the plan's next slices). Each unit's crate is observed in
//! the job's front end, the observations of one package merged, and each finding given once,
//! as the first unit that has its item.
//!
//! The units check without `dead_code` ([`super::compile`] leaves it out of a linted unit's
//! diagnostics): `blaze::dead` says it here, over all of the package's configurations, so an
//! item one of them uses is never called dead in another. A job whose units failed gives none
//! of the three (what a configuration that did not check uses is not known), and says so.
//!
//! Cost: each linted unit's crate is walked once more by the dead-code pass, with what its
//! check inferred still in the front end (a check lets go of nothing, [`super::release`]),
//! plus one walk of the code each configuration does not use. A unit replayed from its record
//! with a cold front end infers its bodies again here: the plan keeps the observation with the
//! record instead.

use blaze_api::{ApiError, Diagnostic};
use blaze_diagnostics::reach::{self, Observation};
use blaze_units::schema::{Mode, Unit};

use super::{Job, UnitReader, cancellable, compile};
use crate::serve::work::Failed;

/// What the job says of its linted packages' items over every configuration they were
/// checked in, `ok` whether every unit built, `built` the units it built.
pub(super) fn report(job: &Job, reader: Option<UnitReader>, ok: bool, built: &[u32]) -> Result<Vec<Diagnostic>, ApiError> {
    let mut linted: Vec<(&str, String, usize)> = Vec::new(); // alloc: once per job, a row per linted unit
    for &unit in built {
        let unit = unit as usize;
        let u = job.unit(unit)?;
        if job.plan(unit)?.clippy.is_none() || u.macro_wasm || !matches!(u.mode, Mode::Check | Mode::Test) {
            continue;
        }
        let Some(label) = label(u) else { continue };
        linted.push((u.pkg_id.as_str(), label, unit));
    }
    if linted.is_empty() {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    if !ok {
        return Ok(vec![Diagnostic::note(
            // alloc: once per job
            "`blaze::dead`, `blaze::test_only` and `blaze::cfg_only` are given only when every unit checks: what a unit \
             that failed uses is not known"
                .to_owned(),
        )]);
    }
    let Some(reader) = reader else {
        let what = "the lints over every configuration are given with no task reading the front end, and one still does";
        return Err(ApiError::new(what, None));
    };
    linted.sort();
    let db = reader.db();
    let mut said: Vec<(u32, blaze_diagnostics::Diag)> = Vec::new(); // alloc: once per job
    let mut notes: Vec<Diagnostic> = Vec::new(); // alloc: none until a note
    for package in linted.chunk_by(|a, b| a.0 == b.0) {
        let _span = tracing::info_span!(target: "blaze", "reachability over configurations", package = package[0].0, configurations = package.len()).entered();
        let mut observations: Vec<Observation> = Vec::with_capacity(package.len()); // alloc: once per package
        let mut crates = Vec::with_capacity(package.len()); // alloc: once per package
        for (_, label, unit) in package {
            let u = job.unit(*unit)?;
            let krate = compile::crate_of(job, *unit, u)?;
            if crates.contains(&krate) {
                continue;
            }
            crates.push(krate);
            let (_, options) = compile::options(job, db, *unit, u)?;
            let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
            let _span = tracing::info_span!(target: "blaze", "reachability observe", unit = label.as_str()).entered();
            // clone: a label, once per configuration observed
            let observation = cancellable(|| reach::observe(db, hir::Crate::from(krate), files, &options, label.clone(), *unit as u32))
                .map_err(|failed| match failed {
                    Failed::Error(error) => error,
                    // alloc: error path
                    Failed::Again => ApiError::new(format!("the job was cancelled as {label} was observed"), Some(label.clone())),
                })?;
            observations.push(observation);
        }
        if let Some((label, why)) = observations.iter().find_map(|it| it.unported.as_ref().map(|why| (&it.label, why))) {
            // alloc: once per package not linted
            notes.push(Diagnostic::note(format!("the reachability lints are not given for `{}`: in {label}, {why}", package[0].0)));
            continue;
        }
        said.extend(reach::merge(&observations).iter().map(|it| (it.unit, it.diag())));
    }
    let diags: Vec<blaze_diagnostics::Diag> = said.iter().map(|it| it.1.clone()).collect(); // clone: once per finding, for the sources they read
    let mut sources = blaze_diagnostics::emit::Sources::default();
    for (name, text) in compile::unmounted(&reader, &job.mounted, &diags) {
        sources.add(&name, Some(text));
    }
    job.mounted.add_sources(&mut sources, &diags);
    let mut style = blaze_diagnostics::emit::Style { ansi: job.input.color, short: job.input.short, ..Default::default() };
    if let Some(width) = job.input.width {
        style.width = usize::try_from(width).unwrap_or(usize::MAX);
    }
    let mut out = notes;
    for (unit, diag) in &said {
        let unit_of = job.unit_of(*unit as usize)?;
        let diagnostic = blaze_diagnostics::api::diagnostic(diag, &sources, style, Some(unit_of))
            // alloc: error path, naming the diagnostic whose span can't be placed
            .map_err(|why| ApiError::new(format!("{why}, in the diagnostic `{}`", diag.message), None))?;
        out.push(diagnostic);
    }
    Ok(out)
}

/// How a finding names a unit's configuration: "lib", "lib (test)", "bin `x`", "test `y`",
/// with its platform when it is not the host's; none for a build script.
fn label(u: &Unit) -> Option<String> {
    let kind = u.target.kind.first().map_or("lib", String::as_str);
    let test = matches!(u.mode, Mode::Test) || u.checked_as_test;
    let mut label = match kind {
        "custom-build" => return None,
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib".to_owned(), // alloc: once per unit
        "test" | "bench" => format!("{kind} `{}`", u.target.name), // alloc: once per unit
        other => format!("{other} `{}`{}", u.target.name, if test { " (test)" } else { "" }), // alloc: once per unit
    };
    if label == "lib" && test {
        label.push_str(" (test)");
    }
    if let Some(platform) = &u.platform {
        label.push_str(" for ");
        label.push_str(platform);
    }
    Some(label)
}
