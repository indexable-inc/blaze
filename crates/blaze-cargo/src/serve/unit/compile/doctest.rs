//! A library's `Doctest` unit (docs/v2/plan/design/doctests.md): every example of its documentation
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
//!
//! Each image (an example's, the runner's) is keyed before it is built by all it is made of
//! ([`base_key`]): the unit's record inputs but its crate's identity, and the code identity of
//! the image's own crate ([`blaze_lower::code_identity`]), which an edit of another example's
//! text (a comment no macro sees, in the library) leaves as it was. Its record, filed under
//! that key and what its code read ([`super::reads`]), names its output and the native inputs
//! its link read. An image whose record is found is [`Linking::Unchanged`]: not checked, built
//! or linked, its file the one the store keeps.

use std::path::Path;

use blaze_api::{ApiError, Level};
use blaze_cas::Key;
use blaze_core::lock::Lock;
use blaze_doc::doctest::{self as doc, Built, Program, Verdict};
use blaze_ids::{ContentId, Hasher};
use blaze_lower::{Appended, DoctestProgram, LowerError, Lowered};

use super::{
    Does, Failed, Image, ImageRecord, Job, Linked, Making, Phases, Put, UnitReader, Written,
    artifact, codegen, header, lock, reads, say, span_unit,
};
use crate::serve::{unit::codegen::Makes, work::Linking as How};

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
        return Err(LowerError {
            what: "a doctest unit with no crate in the front end".to_owned(),
            at: Some(at()),
        });
    };
    if !blaze_lower::macros_loaded(lowered.db(), krate) {
        return Ok(());
    }
    let _span =
        tracing::info_span!(target: "blaze", "doctests made into crates", unit = %at()).entered();
    let package = job.package(u).map_err(|it| LowerError { what: it.what, at: Some(at()) })?;
    let package_dir =
        Path::new(&package.manifest_path).parent().and_then(Path::to_str).ok_or_else(|| {
            // alloc: an error
            LowerError {
                what: format!(
                    "the manifest `{}` is in no directory named in UTF-8",
                    package.manifest_path
                ),
                at: Some(at()),
            }
        })?;
    let programs = {
        let db = lowered.db();
        let library = hir::Crate::from(krate);
        let examples = doc::examples(db, library);
        let of = doc::OfLibrary::read(db, library).map_err(|unnamed| LowerError {
            // alloc: an error
            what: format!("the library {unnamed} has no name for its doctests to use it by"),
            at: Some(at()),
        })?;
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
    // A `test_harness` example is written with the harness its file last had, which lists
    // its tests again when they are unchanged: then its file is written once.
    let last = lowered.doctest_harnesses(unit);
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
            appended: match it.test_harness {
                true => last.iter().find(|(id, _)| *id == it.id).map(|(_, appended)| appended),
                false => None,
            },
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
    // crate as just made and appended to its root, as a test crate's is. Only an example whose
    // listing differs from the harness it was written with (its first build, or an edit of
    // its tests) is written again.
    let harnessed = match asked.iter().any(|it| it.test) {
        true => harnesses(lowered, job, unit, &asked, &made)
            .map_err(|what| LowerError { what, at: Some(at()) })?,
        false => Vec::new(),
    };
    let stale =
        asked.iter().zip(&harnessed).any(|(program, listed)| program.appended != listed.as_ref());
    if stale {
        let _span =
            tracing::info_span!(target: "blaze", "doctest harnesses written again", unit = %at())
                .entered();
        for (program, appended) in asked.iter_mut().zip(&harnessed) {
            program.appended = appended.as_ref();
        }
        made = lowered.doctests(unit, &asked)?;
    }
    let mut made = made.into_iter();
    let crates = programs.iter().map(|it| if it.ignore { None } else { made.next() }).collect();
    let Some(runner) = made.next() else {
        return Err(LowerError {
            what: "the front end made no crate of the doctests' runner".to_owned(),
            at: Some(at()),
        });
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
    let spellings = std::sync::Arc::clone(lowered.spellings());
    let frontend = codegen::frontend_over(
        lowered.db(),
        &paths,
        &spellings,
        &job.mounted,
        codegen::build_time(job, unit),
    );
    let mut texts = Vec::with_capacity(asked.len()); // alloc: once per doctest unit with a test example
    for (program, krate) in asked.iter().zip(made) {
        if !program.test {
            texts.push(None);
            continue;
        }
        let harness = frontend
            .harness(*krate, policy.profile.panic, policy.mode, |_| {
                blaze_usercode::test::fork_server(false)
            })
            // alloc: an error
            .map_err(|it| format!("the `test_harness` doctest `{}`: {}", program.id, it.what))?;
        let Some(harness) = harness else {
            // alloc: an error
            return Err(format!(
                "the `test_harness` doctest `{}` was made with no `cfg(test)`",
                program.id
            ));
        };
        texts.push(Some(Appended { after: harness, module: hir_ty_codegen::v2::HARNESS_MODULE }));
    }
    Ok(texts)
}

/// What [`Phase::Codegen`](super::Phase) made of a `Doctest` unit, for its link.
pub(crate) struct Generated {
    /// The verdict on each program before it runs, by its index; none for an ignored one.
    verdicts: Vec<Option<Verdict>>,
    /// The image of each example linked, by its index.
    examples: Vec<(usize, Code)>,
    /// The runner's image, or why it has none.
    runner: Result<Code, String>,
}

/// [`Phase::Names`](super::Phase) of a `Doctest` unit not kept: no check of its own (the
/// library's is its lib unit's), and its crates made.
pub(super) fn names(job: &Job, unit: usize, slot: &Lock<Phases>) -> Result<(), Failed> {
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
        ApiError::new(
            format!("the examples of {what} were not made into crates before it compiled"),
            None,
        )
    })
}

/// What one example's build made ([`example`]).
enum Example {
    /// Not built: an ignored example.
    Ignored,
    /// Judged before it runs, and not linked: refused, `compile_fail`, or failed to build.
    Judged(Verdict),
    /// Built now or before, and its image.
    Built(Verdict, Code),
}

/// An image of a `Doctest` unit, as [`Phase::Codegen`](super::Phase) has it.
enum Code {
    /// Its code built, to link, and its base key, which its record is filed under once linked.
    Built(codegen::Parts, Key),
    /// Made before from the same inputs: its record.
    Unchanged(Found),
}

/// The record of an image ([`found`]): its output, in the store, and the native inputs its
/// link read, each as it was.
struct Found {
    output: Key,
    natives: Vec<(String, ContentId)>,
}

/// The domain of an image's base key ([`base_key`]).
const IMAGE: &str = "blaze v2 doctest image 1";

/// What stands for the crate identity among the unit's record inputs in an image's base key
/// ([`unit_part`]): each image has its own crate's code identity there.
const NO_IDENTITY: &str = "blaze v2 doctest image without the unit's identity 1";

/// `unit`'s record inputs with no crate identity: what every image of the unit is made with
/// besides its own crate (its profile, platform, flags, toolchain, blaze and the rest).
fn unit_part(job: &Job, unit: usize) -> Result<Key, ApiError> {
    super::with_record_inputs(job, unit, ContentId::of(NO_IDENTITY, b""), |inputs| inputs.key(unit))
}

/// The base key of the image of `krate` named `name` (an example's id, or the runner's): the
/// unit's record inputs `part` and the crate's code identity.
fn base_key(db: &base_db::Db, part: Key, krate: base_db::Crate, name: &str) -> Key {
    let mut hasher = Hasher::new(IMAGE);
    hasher.id(&part.0).id(&blaze_lower::code_identity(db, krate)).part(name.as_bytes());
    Key(hasher.finish())
}

/// The record of the image of base key `base` in the daemon's store, filed under it or under
/// what its code read with the values it has now, its output in the store and every native
/// input as it was: none when there is none. A store that can't be read, or a record that is
/// not an image's, is the error, naming the key.
fn found(job: &Job, base: Key) -> Result<Option<Found>, ApiError> {
    let guard = blaze_cas::enter();
    let Some((_, entry, _)) = super::filed_in(job, &job.input.store, false, &guard, base)? else {
        return Ok(None);
    };
    match header::outputs_natives(entry.bytes) {
        Some((outputs, natives)) if outputs.len() == 1 => {
            Ok(Some(Found { output: outputs[0], natives }))
        }
        // alloc: error path
        _ => Err(ApiError::new(
            format!(
                "the record of doctest image key {:x} in the daemon's store names no one output",
                base.0
            ),
            None,
        )),
    }
}

/// [`Phase::Codegen`](super::Phase) of a `Doctest` unit: each example checked and judged, its
/// code compiled when it builds and is to be linked (a `compile_fail` one is not), and the
/// runner's. The examples are built side by side ([`blaze_graph::par`]), each its own crate,
/// their results in their order. An example blaze refuses to compile fails its own test,
/// naming why; the runner failing fails the unit.
pub(super) fn generate(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    slot: &Lock<Phases>,
) -> Result<(), Failed> {
    let expanded = expanded(job, unit)?;
    let u = job.unit(unit)?;
    let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
    let part = unit_part(job, unit)?;
    let built = blaze_graph::par::map(
        blaze_graph::par::zip(&expanded.programs, &expanded.crates),
        |(program, krate)| match *krate {
            Some(krate) => example(job, reader, unit, part, files, program, krate),
            None => Ok(Example::Ignored),
        },
    );
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
            Example::Built(verdict, code) => {
                verdicts.push(Some(verdict));
                examples.push((at, code));
            }
        }
    }
    let runner = {
        let _span =
            tracing::info_span!(target: "blaze", "doctest runner", unit = %span_unit(u)).entered();
        let base = {
            let _span = tracing::info_span!(target: "blaze", "doctest image key", function = "the doctests' runner").entered();
            let base = base_key(reader.db(), part, expanded.runner, RUNNER);
            found(job, base)?.ok_or(base)
        };
        match base {
            Ok(found) => Ok(Code::Unchanged(found)),
            Err(base) => {
                let made = codegen::generate(
                    job,
                    reader,
                    unit,
                    expanded.runner,
                    &Makes::Executable,
                    &job.mounted,
                    None,
                );
                match made.map_err(|why| ApiError::new(why, Some(span_unit(u))))? {
                    Ok(parts) => Ok(Code::Built(parts, base)),
                    // alloc: error path
                    Err(refused) => Err(format!(
                        "blaze can't build the doctests' runner: {}",
                        refused.errors.join("; ")
                    )),
                }
            }
        }
    };
    if job.cancelled() {
        return Err(Failed::Again);
    }
    lock(slot).doctests = Some(Generated { verdicts, examples, runner });
    Ok(())
}

/// One example of `Doctest` unit `unit`, `program` in `krate`: its image's record found under
/// its base key (with the unit's record inputs `part`), or else checked, judged as rustdoc
/// judges it, and its code compiled when it builds and is to be linked. Its span names the
/// example (`function`) and the unit, and covers all of its work.
fn example(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    part: Key,
    files: blaze_diagnostics::Files<'_>,
    program: &Program,
    krate: base_db::Crate,
) -> Result<Example, Failed> {
    let u = job.unit(unit)?;
    let _span = tracing::info_span!(target: "blaze", "doctest example", unit = %span_unit(u), function = %program.name).entered();
    let db = reader.db();
    // An example that builds and runs is linked, and its image recorded: one recorded under
    // the same key builds as it did, and its verdict before it runs is the one it had.
    let built = program.after_build(Built::Built);
    let linked = !program.compile_fail && matches!(built, Verdict::Run | Verdict::Passed);
    let base = {
        let _span =
            tracing::info_span!(target: "blaze", "doctest image key", function = %program.name)
                .entered();
        let base = base_key(db, part, krate, &program.id);
        match linked {
            true => found(job, base)?.ok_or(base),
            false => Err(base),
        }
    };
    let base = match base {
        Ok(found) => return Ok(Example::Built(built, Code::Unchanged(found))),
        Err(base) => base,
    };
    let diags = {
        let _span =
            tracing::info_span!(target: "blaze", "doctest example check", function = %program.name)
                .entered();
        crate::serve::unit::checks::Checking::new(db, hir::Crate::from(krate))
            .check(db, files, &blaze_diagnostics::Options::default())
            .diags
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
        Ok(Ok(parts)) => Ok(Example::Built(verdict, Code::Built(parts, base))),
        // alloc: a verdict's words, once per example blaze can't build
        Ok(Err(refused)) => Ok(Example::Judged(Verdict::Failed(format!(
            "blaze can't build the example: {}",
            refused.errors.join("; ")
        )))),
        Err(why) => Err(ApiError::new(why, Some(span_unit(u))).into()),
    }
}

/// One image of a `Doctest` unit to link ([`link`]): an example's (`at`, its index) or the
/// runner's, its path, its code, and the link kept at its path from the last build.
struct Linking {
    at: Option<usize>,
    path: String,
    code: Code,
    kept: Vec<Image>,
}

/// What [`link`] made of one built image: the link, and where its record is filed (none when
/// what its code read can't key one, or every slot of its key holds other names).
type LinkedImage = (Result<codegen::LinkMade, codegen::LinkFailed>, Option<reads::Filing>);

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
    let errors: Vec<&blaze_diagnostics::Diag> = diags
        .iter()
        .filter(|it| it.level == blaze_diagnostics::model::Level::Error && !super::closing(it))
        .collect();
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
        match blaze_diagnostics::emit::rendered(
            diag,
            &sources,
            blaze_diagnostics::emit::Style::default(),
        ) {
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
pub(super) fn link(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phases: &mut Phases,
    images: &mut Vec<Image>,
) -> Result<(), Failed> {
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
        return Err(ApiError::new(
            format!("{} plans no program for its runner", span_unit(u)),
            None,
        )
        .into());
    };
    let mut making = Making {
        // alloc: none until a refusal
        diagnostics: Vec::new(),
        ok: true,
        open: false,
        why: None,
        made: super::Made::default(),
        nothing: false,
        files: Vec::new(),
        puts: Vec::new(),
        natives: Vec::new(),
        links: Vec::new(),
        macros: None,
        records: Vec::new(),
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
    built.push(Linking { at: None, path: main.clone(), code: runner, kept: Vec::new() });
    for (at, code) in examples.drain(..) {
        let program = &expanded.programs[at];
        let exe = format!("{dir}/{}{}", program.id, std::env::consts::EXE_SUFFIX); // alloc: once per example linked
        built.push(Linking { at: Some(at), path: exe, code, kept: Vec::new() });
    }
    for it in &mut built {
        if matches!(it.code, Code::Built(..)) {
            // alloc: once per image linked, the one link kept at its path
            it.kept = images.extract_if(.., |kept| kept.path == it.path).collect();
        }
    }
    let env = &job.input.config.invocation.env;
    let made: Vec<Option<LinkedImage>> = blaze_graph::par::map(&mut built, |it| {
        let Code::Built(parts, base) = &it.code else { return None };
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
        let generated = parts.over(reader, &job.mounted);
        let linked =
            codegen::link(job, unit, krate, &it.path, &Makes::Executable, &generated, &mut it.kept);
        // Filed by what the image's code read, as a unit record is: known now it is built.
        let filing = match &linked {
            Ok(_) => {
                let _span =
                    tracing::info_span!(target: "blaze", "doctest image record", function = %name)
                        .entered();
                let guard = blaze_cas::enter();
                let filed = reads::Reads::of(reader.db(), krate).and_then(|read| {
                    reads::file(
                        *base,
                        &read,
                        env,
                        &|manifest| job.units().is_primary_manifest(manifest),
                        |slot| job.input.store.get(&guard, slot).ok().flatten().map(|it| it.bytes),
                    )
                });
                match filed {
                    Ok(filing) => filing,
                    Err(why) => {
                        tracing::info!(image = %it.path, "doctest image record not kept: {why}");
                        None
                    }
                }
            }
            Err(_) => None,
        };
        Some((linked, filing))
    });
    for it in &mut built {
        images.append(&mut it.kept);
    }
    if job.cancelled() {
        return Err(Failed::Again);
    }
    for (it, made) in built.iter().zip(made) {
        if let Code::Unchanged(found) = &it.code {
            // clone: once per image unchanged, its few native inputs
            making.natives.extend(found.natives.iter().cloned());
            // alloc: once per image unchanged
            making.links.push((it.path.clone(), How::Unchanged));
            making.files.push((it.path.clone(), Written::Kept(found.output), true)); // clone: as above
            continue;
        }
        let Some((made, filing)) = made else { continue };
        match made {
            Ok(linked) => {
                if let Some(filing) = filing {
                    // alloc: once per image recorded, its path and few native inputs
                    making.records.push(ImageRecord {
                        path: it.path.clone(),
                        filing,
                        natives: linked.read.clone(),
                    });
                }
                making.natives.extend(linked.read);
                // alloc: once per image linked
                making.links.push((it.path.clone(), linked.how));
                making.files.push((it.path.clone(), Written::Bytes(linked.bytes), true)); // clone: as above
            }
            Err(failed) => match it.at {
                // alloc: a verdict's words, once per example that fails to link
                Some(at) => {
                    verdicts[at] =
                        Some(Verdict::Failed(format!("blaze can't link the example: {failed}")))
                }
                None => {
                    making.diagnostics.push(say::refused(failed.to_string(), Some(unit_of), color));
                    making.ok = false;
                    making.nothing = true;
                    making.files.clear();
                    making.records.clear();
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
    making.files.push((
        format!("{dir}/{}", doc::VERDICTS),
        Written::Bytes(lines.into_bytes()),
        false,
    ));
    // Moved, not copied: the store keeps them as they are.
    let subject = super::unit_subject(job, u);
    let mut used = Vec::new(); // alloc: once per doctest unit linked
    for Linking { code, .. } in &mut built {
        let Code::Built(parts, _) = code else { continue };
        let puts = std::mem::take(&mut parts.built.puts);
        making.puts.extend(puts.into_iter().map(|it| Put {
            key: it.key,
            kind: it.kind,
            bytes: it.bytes,
        }));
        for mut miss in std::mem::take(&mut parts.built.misses) {
            match &subject {
                Ok(subject) => miss.subject = super::within(*subject, miss.subject),
                Err(why) => {
                    miss.subject = blaze_ids::ContentId::default();
                    // alloc: error path, once a miss
                    miss.missed =
                        blaze_kernel::Missed::Failed(format!("its unit has no subject: {why}"));
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
    path.strip_prefix(main).is_some_and(|rest| {
        rest.starts_with(doc::DIR_SUFFIX) && rest[doc::DIR_SUFFIX.len()..].starts_with('/')
    })
}
