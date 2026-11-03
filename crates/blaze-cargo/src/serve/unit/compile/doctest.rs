//! A library's `Doctest` unit (docs/v2/plan/doctests.md): every example of its documentation
//! is a crate and a program of its own, built and run as its own process, and the unit's
//! program is the runner, a libtest harness with one test per example. blaze never runs
//! rustdoc and never merges examples.
//!
//! The loop's writer makes the crates ([`expand`], from [`super::super::harness::write`]):
//! each example's program as rustdoc writes it ([`doc::programs`]) and the runner
//! ([`doc::runner`]). The unit's phases then build them:
//! - [`Phase::Names`](super::Phase): its record replayed when kept, else nothing to read: the
//!   library's own check is its lib unit's.
//! - [`Phase::Codegen`](super::Phase): each example checked, judged as rustdoc judges it
//!   ([`Program::after_build`]), and its code compiled when it is to be linked; the runner's
//!   code compiled.
//! - [`Phase::Link`](super::Phase): each example linked to `<runner>.doctests/<id>`, the
//!   runner to the unit's program, and the verdicts written beside them. A kept image of an
//!   example is relinked at its own path, so an edit of one example patches that one.

use std::path::Path;

use blaze_api::{ApiError, Level};
use blaze_doc::doctest::{self as doc, Built, Program, Verdict};
use blaze_lower::{Appended, DoctestProgram, LowerError, Lowered};
use parking_lot::Mutex;

use super::{Does, Failed, Image, Job, Linked, Making, Phases, Put, UnitReader, artifact, codegen, lock, say, span_unit};
use crate::serve::unit::codegen::Makes;

/// The id of the runner's crate among a unit's doctest crates: no example's (rustdoc's are a
/// file's name, flattened, a line and a number).
const RUNNER: &str = "blaze doctest runner";

/// A `Doctest` unit's programs and their crates, made on the loop's writer ([`expand`]).
pub(crate) struct Expanded {
    programs: Vec<Program>,
    /// The crate of each of `programs`, by its index; none for an ignored one, which is not
    /// built.
    crates: Vec<Option<base_db::Crate>>,
    runner: base_db::Crate,
}

/// The crates of `unit`'s examples and runner made in `lowered`, once a job, when `unit` is a
/// `Doctest` unit whose crate's proc macros are loaded: what the examples are found from is
/// the library as its macros expand it. One whose macros are not loaded yet is left, and made
/// when they are ([`super::super::harness::reached`]). An error names the unit and why.
pub(crate) fn expand(lowered: &mut Lowered, job: &Job, unit: usize) -> Result<(), LowerError> {
    let Some(slot) = job.doctests.get(unit) else { return Ok(()) };
    if slot.get().is_some() {
        return Ok(());
    }
    let u = job.unit(unit).map_err(|it| LowerError { what: it.what, at: None })?;
    let at = || span_unit(u);
    let Some(krate) = lowered.crates().krate(unit) else {
        return Err(LowerError { what: "a doctest unit with no crate in the front end".to_owned(), at: Some(at()) });
    };
    if !blaze_lower::macros_loaded(lowered.db(), krate) {
        return Ok(());
    }
    let _span = tracing::info_span!(target: "blaze", "doctests made into crates", unit = %at()).entered();
    let package = job.package(u).map_err(|it| LowerError { what: it.what, at: Some(at()) })?;
    let package_dir = Path::new(&package.manifest_path).parent().and_then(Path::to_str).ok_or_else(|| {
        // alloc: an error
        LowerError { what: format!("the manifest `{}` is in no directory named in UTF-8", package.manifest_path), at: Some(at()) }
    })?;
    let programs = {
        let db = lowered.db();
        let library = hir::Crate::from(krate);
        let examples = doc::examples(db, library);
        let of = doc::OfLibrary::read(db, library);
        doc::programs(&of, &examples, codegen::triple(job, u), package_dir)
    };
    let runner = doc::runner(&programs);
    // An example's file is named by the file it is in, so its errors and panics point there.
    let named: Vec<String> = programs
        .iter()
        .map(|it| match Path::new(&it.file).is_absolute() {
            true => it.file.clone(), // clone: a file's name, once per example
            false => format!("{package_dir}/{}", it.file), // alloc: as above
        })
        .collect(); // alloc: once per doctest unit made
    let runner_named = format!("{package_dir}/blaze-doctest-runner.rs"); // alloc: once per doctest unit made
    let mut asked: Vec<DoctestProgram<'_>> = programs
        .iter()
        .zip(&named)
        .filter(|(it, _)| !it.ignore)
        .map(|(it, named)| DoctestProgram {
            id: &it.id,
            named,
            source: &it.source,
            edition: it.edition,
            test: it.test_harness,
            appended: None,
        })
        .collect(); // alloc: once per doctest unit made
    let runner_program = DoctestProgram {
        id: RUNNER,
        named: &runner_named,
        source: &runner,
        edition: syntax::Edition::Edition2021,
        test: false,
        appended: None,
    };
    asked.push(runner_program);
    let mut made = lowered.doctests(unit, &asked)?;
    // A `test_harness` example is a test crate, as rustdoc's `--test` builds it: its harness
    // (each `#[test]` of it listed for libtest, and a `main` that runs them) is listed from its
    // crate as just made and appended to its root, as a test crate's is, which is then
    // written again.
    let harnessed = match asked.iter().any(|it| it.test) {
        true => harnesses(lowered, job, unit, &asked, &made).map_err(|what| LowerError { what, at: Some(at()) })?,
        false => Vec::new(),
    };
    if !harnessed.is_empty() {
        for (program, appended) in asked.iter_mut().zip(&harnessed) {
            program.appended = appended.as_ref();
        }
        made = lowered.doctests(unit, &asked)?;
    }
    let mut made = made.into_iter();
    let crates = programs.iter().map(|it| if it.ignore { None } else { made.next() }).collect();
    let Some(runner) = made.next() else {
        return Err(LowerError { what: "the front end made no crate of the doctests' runner".to_owned(), at: Some(at()) });
    };
    let _ = slot.set(Expanded { programs, crates, runner });
    Ok(())
}

/// The harness of each of `asked` that is a test (`test_harness`), its crate `made`, to append
/// to its root as a test crate's is ([`super::super::harness::write`]); none for any other. The harness never serves a fork
/// server (the runner starts each example as a process of its own) and records nothing. An
/// error names the example and why.
fn harnesses(
    lowered: &Lowered,
    job: &Job,
    unit: usize,
    asked: &[DoctestProgram<'_>],
    made: &[base_db::Crate],
) -> Result<Vec<Option<Appended>>, String> {
    let toolchain = codegen::toolchain(job, unit)?;
    let policy = codegen::policy(job, unit, toolchain)?;
    let paths = std::sync::Arc::clone(lowered.paths());
    let frontend = codegen::frontend_over(lowered.db(), &paths, &job.mounted, codegen::build_time(job, unit));
    let mut texts = Vec::with_capacity(asked.len()); // alloc: once per doctest unit with a test example
    for (program, krate) in asked.iter().zip(made) {
        if !program.test {
            texts.push(None);
            continue;
        }
        let harness = frontend
            .harness(*krate, policy.profile.panic, policy.mode, |_| blaze_usercode::test::fork_server(false))
            // alloc: an error
            .map_err(|it| format!("the `test_harness` doctest `{}`: {}", program.id, it.what))?;
        let Some(harness) = harness else {
            // alloc: an error
            return Err(format!("the `test_harness` doctest `{}` was made with no `cfg(test)`", program.id));
        };
        texts.push(Some(Appended { after: harness, module: hir_ty_codegen::v2::HARNESS_MODULE }));
    }
    Ok(texts)
}

/// What [`Phase::Codegen`](super::Phase) made of a `Doctest` unit, for its link.
pub(crate) struct Generated {
    /// The verdict on each program before it runs, by its index; none for an ignored one.
    verdicts: Vec<Option<Verdict>>,
    /// The code of each example to link, by its index.
    examples: Vec<(usize, codegen::Parts)>,
    /// The runner's code, or why it has none.
    runner: Result<codegen::Parts, String>,
}

/// [`Phase::Names`](super::Phase) of a `Doctest` unit not kept: no check of its own (the
/// library's is its lib unit's), and its crates made.
pub(super) fn names(job: &Job, unit: usize, slot: &Mutex<Phases>) -> Result<(), Failed> {
    expanded(job, unit)?;
    let mut phases = lock(slot);
    phases.does = Some(Does { inert: false, builds_code: true });
    // alloc: none
    (phases.checking, phases.diags, phases.unmounted) = (None, Some(Vec::new()), Vec::new());
    Ok(())
}

fn expanded(job: &Job, unit: usize) -> Result<&Expanded, ApiError> {
    job.doctests.get(unit).and_then(|it| it.get()).ok_or_else(|| {
        let what = match job.unit(unit) {
            Ok(u) => span_unit(u),
            Err(error) => error.what,
        };
        // alloc: error path
        ApiError::new(format!("the examples of {what} were not made into crates before it compiled"), None)
    })
}

/// What one example's build made ([`example`]).
enum Example {
    /// Not built: an ignored example.
    Ignored,
    /// Judged before it runs, and not linked: refused, `compile_fail`, or failed to build.
    Judged(Verdict),
    /// Built, and its code to link.
    Built(Verdict, codegen::Parts),
}

/// [`Phase::Codegen`](super::Phase) of a `Doctest` unit: each example checked and judged, its
/// code compiled when it builds and is to be linked (a `compile_fail` one is not), and the
/// runner's. The examples are built side by side ([`blaze_graph::par`]), each its own crate,
/// their results in their order. An example blaze refuses to compile fails its own test,
/// naming why; the runner failing fails the unit.
pub(super) fn generate(job: &Job, reader: &UnitReader, unit: usize, slot: &Mutex<Phases>) -> Result<(), Failed> {
    let expanded = expanded(job, unit)?;
    let u = job.unit(unit)?;
    let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
    let built = blaze_graph::par::map(blaze_graph::par::zip(&expanded.programs, &expanded.crates), |(program, krate)| match *krate {
        Some(krate) => example(job, reader, unit, files, program, krate),
        None => Ok(Example::Ignored),
    });
    if job.cancelled() {
        return Err(Failed::Again);
    }
    // alloc: once per doctest unit built
    let mut verdicts = Vec::with_capacity(built.len());
    let mut examples = Vec::new(); // alloc: once per doctest unit built
    for (at, made) in built.into_iter().enumerate() {
        match made? {
            Example::Ignored => verdicts.push(None),
            Example::Judged(verdict) => verdicts.push(Some(verdict)),
            Example::Built(verdict, parts) => {
                verdicts.push(Some(verdict));
                examples.push((at, parts));
            }
        }
    }
    let runner = {
        let _span = tracing::info_span!(target: "blaze", "doctest runner", unit = %span_unit(u)).entered();
        let made = codegen::generate(job, reader, unit, expanded.runner, &Makes::Executable, &job.mounted, None);
        match made.map_err(|why| ApiError::new(why, Some(span_unit(u))))? {
            Ok(parts) => Ok(parts),
            // alloc: error path
            Err(errors) => Err(format!("blaze can't build the doctests' runner: {}", errors.join("; "))),
        }
    };
    if job.cancelled() {
        return Err(Failed::Again);
    }
    lock(slot).doctests = Some(Generated { verdicts, examples, runner });
    Ok(())
}

/// One example of `Doctest` unit `unit`, `program` in `krate`: checked, judged as rustdoc
/// judges it, and its code compiled when it builds and is to be linked. Its span names the
/// example (`function`) and the unit, and covers all of its work.
fn example(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    files: blaze_diagnostics::Files<'_>,
    program: &Program,
    krate: base_db::Crate,
) -> Result<Example, Failed> {
    let u = job.unit(unit)?;
    let _span = tracing::info_span!(target: "blaze", "doctest example", unit = %span_unit(u), function = %program.name).entered();
    let db = reader.db();
    let diags = {
        let _span = tracing::info_span!(target: "blaze", "doctest example check", function = %program.name).entered();
        crate::serve::unit::checks::Checking::new(db, hir::Crate::from(krate)).check(db, files, &blaze_diagnostics::Options::default())
    };
    let verdict = match refused(job, files, krate, reader, &diags) {
        Ok(None) => program.after_build(Built::Built),
        Ok(Some(errors)) => program.after_build(Built::Refused { errors: &errors }),
        Err(why) => Verdict::Failed(why),
    };
    if program.compile_fail || !matches!(verdict, Verdict::Run | Verdict::Passed) {
        return Ok(Example::Judged(verdict));
    }
    let made = {
        let _span = tracing::info_span!(target: "blaze", "doctest example codegen", function = %program.name).entered();
        codegen::generate(job, reader, unit, krate, &Makes::Executable, &job.mounted, None)
    };
    if job.cancelled() {
        return Err(Failed::Again);
    }
    match made {
        Ok(Ok(parts)) => Ok(Example::Built(verdict, parts)),
        // alloc: a verdict's words, once per example blaze can't build
        Ok(Err(errors)) => Ok(Example::Judged(Verdict::Failed(format!("blaze can't build the example: {}", errors.join("; "))))),
        Err(why) => Err(ApiError::new(why, Some(span_unit(u))).into()),
    }
}

/// One image of a `Doctest` unit to link ([`link`]): an example's (`at`, its index) or the
/// runner's, its path, its code, and the link kept at its path from the last build.
struct Linking {
    at: Option<usize>,
    path: String,
    parts: codegen::Parts,
    kept: Vec<Image>,
}

/// The errors `diags` of an example's check, its crate `krate`, rendered as rustc writes them
/// (`error[E0308]: ..`), the crate's own text standing for the file it is named by; none when
/// it has none. An
/// error that can't be placed in its file is why it can't be judged.
fn refused(
    job: &Job,
    files: blaze_diagnostics::Files<'_>,
    krate: base_db::Crate,
    reader: &UnitReader,
    diags: &[blaze_diagnostics::Diag],
) -> Result<Option<String>, String> {
    let errors: Vec<&blaze_diagnostics::Diag> =
        diags.iter().filter(|it| it.level == blaze_diagnostics::model::Level::Error && !super::closing(it)).collect();
    if errors.is_empty() {
        return Ok(None);
    }
    let db = reader.db();
    let mut sources = blaze_diagnostics::emit::Sources::default();
    // The example's text first, as its crate was made of it (a `test_harness` example's with
    // its harness): the file of its name on disk is the documentation's.
    let root = krate.data(db).root_file_id;
    sources.add(&files.name(root), Some(db.file_text(root).text(db).to_string())); // alloc: an example's text, once per refused example
    job.mounted.add_sources(&mut sources, diags);
    let mut text = String::new();
    for diag in errors {
        match blaze_diagnostics::emit::rendered(diag, &sources, blaze_diagnostics::emit::Style::default()) {
            Ok(it) => {
                text.push_str(&it);
                text.push('\n');
            }
            // alloc: a verdict's words
            Err(why) => return Err(format!("blaze can't show the example's errors: {why}")),
        }
    }
    Ok(Some(text))
}

/// [`Phase::Link`](super::Phase) of a `Doctest` unit: each example's program linked at
/// `<runner>.doctests/<id>` (relinking its kept image from `images`), the runner at the
/// unit's program, and the verdicts beside the examples. What codegen put, missed and used
/// of the memo is moved from each build into the making and the job.
pub(super) fn link(job: &Job, reader: &UnitReader, unit: usize, phases: &mut Phases, images: &mut Vec<Image>) -> Result<(), Failed> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let unit_of = job.unit_of(unit)?;
    let color = job.input.color;
    let Some(Generated { mut verdicts, mut examples, runner }) = phases.doctests.take() else {
        // alloc: error path
        let what = format!("{} is linked before its examples are built", span_unit(u));
        return Err(ApiError::new(what, None).into());
    };
    let expanded = expanded(job, unit)?;
    let Some(main) = artifact::main(u, plan) else {
        // alloc: error path
        return Err(ApiError::new(format!("{} plans no program for its runner", span_unit(u)), None).into());
    };
    let mut making = Making {
        // alloc: none until a refusal
        diagnostics: Vec::new(),
        ok: true,
        made: super::Made::default(),
        nothing: false,
        files: Vec::new(),
        puts: Vec::new(),
        natives: Vec::new(),
        links: Vec::new(),
        macros: None,
    };
    let runner = match runner {
        Ok(it) => it,
        Err(why) => {
            making.diagnostics.push(say::refused(why, Some(unit_of), color));
            making.ok = false;
            making.nothing = true;
            phases.linked = Some(Linked::Made(Box::new(making)));
            return Ok(());
        }
    };
    let dir = format!("{main}{}", doc::DIR_SUFFIX); // alloc: once per doctest unit linked
    // The runner first: the unit's main file, which its dep-info is of. Each image with the
    // kept link at its path, taken out of `images`, so the links run side by side
    // ([`blaze_graph::par`]), each its own image; the kept links go back once all are made.
    let mut built: Vec<Linking> = Vec::with_capacity(examples.len() + 1); // alloc: as `dir`
    // clone: the runner's path, once
    built.push(Linking { at: None, path: main.clone(), parts: runner, kept: Vec::new() });
    for (at, parts) in examples.drain(..) {
        let program = &expanded.programs[at];
        let exe = format!("{dir}/{}{}", program.id, std::env::consts::EXE_SUFFIX); // alloc: once per example linked
        built.push(Linking { at: Some(at), path: exe, parts, kept: Vec::new() });
    }
    for it in &mut built {
        // alloc: once per image linked, the one link kept at its path
        it.kept = images.extract_if(.., |kept| kept.path == it.path).collect();
    }
    let made = blaze_graph::par::map(&mut built, |it| {
        if job.cancelled() {
            return None;
        }
        let krate = match it.at {
            Some(at) => expanded.crates[at],
            None => Some(expanded.runner),
        };
        let krate = krate?;
        let name = it.at.map_or("the doctests' runner", |at| expanded.programs[at].name.as_str());
        let _span = tracing::info_span!(target: "blaze", "doctest example link", unit = %span_unit(u), function = %name).entered();
        let generated = it.parts.over(reader, &job.mounted);
        Some(codegen::link(job, unit, krate, &it.path, &Makes::Executable, &generated, &mut it.kept))
    });
    for it in &mut built {
        images.append(&mut it.kept);
    }
    if job.cancelled() {
        return Err(Failed::Again);
    }
    for (it, made) in built.iter().zip(made) {
        let Some(made) = made else { continue };
        match made {
            Ok(linked) => {
                making.natives.extend(linked.read);
                // alloc: once per image linked
                making.links.push((it.path.clone(), linked.how));
                making.files.push((it.path.clone(), linked.bytes, true)); // clone: as above
            }
            Err(failed) => match it.at {
                // alloc: a verdict's words, once per example that fails to link
                Some(at) => verdicts[at] = Some(Verdict::Failed(format!("blaze can't link the example: {failed}"))),
                None => {
                    making.diagnostics.push(say::refused(failed.to_string(), Some(unit_of), color));
                    making.ok = false;
                    making.nothing = true;
                    making.files.clear();
                    phases.linked = Some(Linked::Made(Box::new(making)));
                    return Ok(());
                }
            },
        }
    }
    let mut lines = String::new();
    for (program, verdict) in expanded.programs.iter().zip(&verdicts) {
        if let Some(verdict) = verdict {
            lines.push_str(&doc::verdict_line(program, verdict));
        }
    }
    // alloc: once per doctest unit linked
    making.files.push((format!("{dir}/{}", doc::VERDICTS), lines.into_bytes(), false));
    // Moved, not copied: the store keeps them as they are.
    let subject = super::unit_subject(job, u);
    let mut used = Vec::new(); // alloc: once per doctest unit linked
    for Linking { parts, .. } in &mut built {
        let puts = std::mem::take(&mut parts.built.puts);
        making.puts.extend(puts.into_iter().map(|it| Put { key: it.key, kind: it.kind, bytes: it.bytes }));
        for mut miss in std::mem::take(&mut parts.built.misses) {
            match &subject {
                Ok(subject) => miss.subject = super::within(*subject, miss.subject),
                Err(why) => {
                    miss.subject = blaze_ids::ContentId::default();
                    // alloc: error path, once a miss
                    miss.missed = blaze_kernel::Missed::Failed(format!("its unit has no subject: {why}"));
                    miss.parts.clear();
                }
            }
            phases.misses.push(miss);
        }
        used.extend(std::mem::take(&mut parts.built.memo_used));
    }
    if !used.is_empty()
        && let Some(slot) = job.memo_used.get(unit)
    {
        used.sort_unstable();
        used.dedup();
        let _ = slot.set(used);
    }
    if making.diagnostics.iter().any(|it| it.level == Level::Error) {
        making.ok = false;
    }
    phases.linked = Some(Linked::Made(Box::new(making)));
    Ok(())
}

/// Whether `path`, an output of a `Doctest` unit whose program is at `main`, is one of its
/// examples' or their verdicts: kept with the unit's record beside its planned outputs.
pub(crate) fn beside(main: &str, path: &str) -> bool {
    path.strip_prefix(main).is_some_and(|rest| rest.starts_with(doc::DIR_SUFFIX) && rest[doc::DIR_SUFFIX.len()..].starts_with('/'))
}
