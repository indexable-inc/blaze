//! `blaze run`'s program, after its check: the package's source files read through bstd and
//! handed to the walking skeleton's straight line (`blaze_ladder::run::run_program`: the new
//! front end over the toolchain's shelf), which places the code in this process and calls
//! `main` (crates/blaze-cli/tests/ladder/README.md, route (e)). A reached function this build
//! cannot make is a stub: the program runs, and stops by name only if it calls one. Which
//! process the program runs in is decided first, before anything is built
//! (`blaze_ladder::run::entry_start`, the program's start as the launcher's form): every
//! program goes to its own process (own_process.rs; run.md "Every program runs in the child")
//! and is never built here. The in-process run (`run_in_process`) is dead, kept until the
//! switch deletes it.

use core::ffi::c_void;
use core::ptr::NonNull;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, OverflowChecks, RunError};
use blaze_manifest::layout::panic;
use blaze_manifest::layout::profile::{self, BuiltinProfile};
use blaze_manifest::layout::ManifestId;
use blaze_own::Vec;
use bstd::Cx;
use bstd::env::VarName;
use bstd::fs::{Dir, ReadWrite};
use bstd::hosting::Hosting;
use bstd::io::Io;
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::sweep::{self, LeafKind, Rows};

/// What `blaze run` hands the program: the command's world, which program (`-p`, `--bin`:
/// run_package.rs), the grants named on its command line (`--grant name=value`, run.md), and
/// the words after `--`.
pub struct Launch<'a> {
    pub io: &'a Io,
    pub select: crate::run_package::Select<'a>,
    pub grants: &'a [(&'a str, &'a str)],
    pub args: &'a [&'a str],
}

/// The bytes of the exchange a hosted program's calls pass through (bstd `Hosting`).
const EXCHANGE: usize = 4096;

/// The program's source files, each path with its text, sorted by path: the files the
/// request's walk opened (`rows`' slots that are present, from `Effect::Open` this command or
/// the leaf table's rows on a repeat) under `under`, the folder of the program's crate root
/// (run_package.rs `Program::sources`: `src/` for the root package's). Nothing is listed: what
/// else the checkout holds (a `target/` of a million entries) is never looked at, so the cost
/// is the program's own files' (rule 77). The one home of the set, for the run and for the
/// program's refusals (`main.rs`, `program_refusals`).
///
/// `tables` are the rows to read them from: while the request runs (the program starts before
/// the answer), the last answer's rows as followed and this command's opens, a file in both
/// taken once; after the answer, the rows it kept. A row of a file no `mod` reaches any more is
/// read and never used: the runner walks from the program's crate root.
pub fn sources<'f>(cx: &Cx, root: &Dir<ReadWrite>, under: &str, tables: &[&Rows<'_>], frame: &'f Frame) -> Result<Vec<(&'f str, &'f str), &'f Frame>, CliError> {
    let mut paths: Vec<&'f str, &'f Frame> = Vec::new_in(frame);
    for rows in tables {
        for leaf in rows.of(LeafKind::Slot).filter(|leaf| !leaf.absent() && leaf.flags & sweep::BOTH == 0) {
            let real = sweep::real_path(rows.path(leaf), leaf.flags, frame);
            let path = core::str::from_utf8(real.leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
            if path.starts_with(under) {
                paths.push(path);
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    let mut texts: Vec<(&'f str, &'f str), &'f Frame> = Vec::with_capacity_in(paths.len(), frame);
    for path in paths.iter().copied() {
        let mut bytes = Bytes::new_in(frame, "a source file of the program");
        root.read_all(cx, &Path::new(path)?, &mut bytes)?;
        let text = core::str::from_utf8(Vec::from_slice_in(bytes.as_slice(), frame).leak())
            .map_err(|_| CliError::Text { what: "a source file of the program is not UTF-8" })?;
        texts.push((path, text));
    }
    Ok(texts)
}

/// Runs the package at `root`: the status `main` ended with. A hosted program (its entry bstd's
/// `bstd_hosted_start`) reaches the world only through the host table made here, over the
/// command's `Io` and only what `launch` grants; the host lives until the program has returned.
///
/// Its files are the root package's `src/` folder, listed: the program starts beside its check
/// (`Gate::Beside`), before the walk has opened a file, so the walk's rows ([`sources`]) are
/// not there yet on a first run. Only `src/` is listed, never the checkout: a `target/` of a
/// million entries beside it costs nothing (it filled the command's frame when the whole
/// checkout was listed). Owed (drive.md, "RUNNER 11"): the runner walks forward from the
/// program's root (run_package.rs `Program::root`) through a loader, so a file no `mod` reaches
/// is never read.
///
/// `picked` is the program `request` picked once (run_package.rs `pick`, main.rs): never picked
/// again here, so the manifests are read once a command (rule 77).
///
/// `units` and `native` are the keys `Effect::StartProgram` carried: the units' record that
/// names the entry and the `Native` record holding the program's image. They are taken here so
/// the runner never reads them back from the request; nothing loads the image from the store
/// yet (owed: the image path, A2b's ruling on A6b), so today's straight line does not read them.
///
/// `producer` is the code builder's identity from the command's producer table (D105): the
/// record walk's code keys hold it.
#[expect(unused_variables, reason = "`units` and `native` wait for the image path that loads the program from its records")]
#[expect(clippy::too_many_arguments, reason = "the run's edge: each argument one handle or fact of it")]
pub(crate) fn run<'h>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    launch: &Launch<'_>,
    picked: &crate::run_package::Program<'_>,
    units: blaze_drive::Key,
    native: blaze_drive::Key,
    producer: blaze_ids::producer::ProducerId<blaze_ids::producer::CodeBuilder>,
    // The command's one store: a Rust program's code entries are found and kept there (own_process.rs).
    lent: &crate::store::CommandLent,
    // The request's certificate and the start list's name (start_list.rs): own_process.rs reads
    // and writes the list under them.
    today: crate::start_list::Today,
    // The names records the roots and the maker read, opened on the first need of them, and
    // the body ids (run_ids.rs) made on the first function stepped: main.rs's, each called at
    // most once (run_parent.rs `CommandFront`).
    names: &mut dyn FnMut() -> Result<blaze_ladder::run::make_one::NamesRecords<'h>, crate::run_parent::ParentError>,
    ids: &mut dyn FnMut() -> Result<crate::run_ids::RunIds<'h>, crate::run_parent::ParentError>,
    // The run's closing count, for the profile (own_process.rs `run_own`).
    counted: &mut Option<crate::run_parent::Tally>,
    // The sink the run's events go to (blaze-screen `Watch`): main.rs's live line on a
    // terminal, else `Unwatched`.
    watch: &dyn blaze_screen::Watch,
    heap: &'static blaze_own::Heap,
    frame: &Frame,
) -> Result<u8, CliError> {
    let cx = cx.span("blaze run: the program");
    let (package, edition) = (picked.package, picked.edition);
    // What the records are made for: the edition, the `dev` profile's overflow checks and
    // panic strategy, the code builder (D105), this machine.
    let overflow = dev_overflow(&cx, root, frame)?;
    let panic = dev_panic(&cx, root, frame)?;
    let isa = crate::run_parent::isa().map_err(|why| CliError::OwnProcess(crate::own_process::OwnError::Parent(why)))?;
    let builder = crate::run_parent::builder_for(edition, overflow, panic, producer, isa).map_err(|why| CliError::OwnProcess(crate::own_process::OwnError::Parent(why)))?;
    // EVERY program runs in its own process under the lazy parent (own_process.rs, D103, D124;
    // run.md "Every program runs in the child"), and nothing of it is built here: its start
    // (the roots: bstd's hosted entry, a C `main`, the shim of a Rust program with std, or
    // `main` called directly) is read from the names records only when today's start list does
    // not give it, and each function is found or made at its first call. Its status is the
    // program's, unchanged; a failure of the parent's own is `CliError::OwnProcess`, which
    // `run` ends with `NOT_RUN` (main.rs).
    let roots = blaze_own::Erased::new(frame);
    let front = crate::run_parent::CommandFront::new(names, ids, builder, heap.alloc(), roots.handle(), watch);
    crate::own_process::run_own(&cx, launch, package, builder, front, lent, today, counted, frame).map_err(CliError::OwnProcess)
}

/// The in-process run of a program (blaze-ladder `run_program_given`): DEAD since every
/// program runs in the launcher's child (run.md "Every program runs in the child"); kept, with
/// what it builds (the argument vector, the host table over `launch`'s grants), until the
/// switch deletes it with `run_program_given`.
#[expect(dead_code, reason = "the in-process run, deleted at the switch to the lazy parent (run.md, \"Every program runs in the child\")")]
fn run_in_process(cx: &Cx, launch: &Launch<'_>, files: &[SourceFile<'_>], program_root: &str, new: NewFront<'_>, package: &str, heap: &'static blaze_own::Heap, frame: &Frame) -> Result<u8, CliError> {
    let shared = heap.shared();
    // The program's command line, its name first (run.md "What a program is given"; bstd
    // `Args`: "0 being the program's name"; the run parent's child gets `arg0: package` the
    // same way), made once in the command's frame (D102): the one vector both starts read: a
    // hosted program's `args` grant through the host, a C `main`'s `argc` and `argv` through
    // the runner ([`blaze_ladder::run::Given`]). It lives until the program has returned.
    let argv_alloc = blaze_own::Erased::new(frame);
    let mut argv = blaze_ladder::argv::Argv::new_in(package, launch.args, argv_alloc.handle()).map_err(|e| CliError::Program(RunError::Args(e)))?;
    // bstd's host takes its arguments and grants as owned strings: once per run, Global.
    let args = argv.strings().map(std::string::String::from).collect();
    // `--grant <name>=<value>` is the design's order; the host's list is `(kind, name)`
    // (host_grants.rs): read each, refusing a scope the hosted wire cannot carry.
    let grants = launch
        .grants
        .iter()
        .enumerate()
        .map(|(at, (name, value))| {
            crate::host_grants::host_grant(at + 1, name, value)
                .map(|(kind, name)| (std::string::String::from(kind), std::string::String::from(name)))
        })
        .collect::<Result<std::vec::Vec<_>, _>>()
        .map_err(CliError::HostGrant)?;
    // clone: the command's world, a refcount.
    let hosting = Hosting::new(launch.io.clone(), args, grants, EXCHANGE, shared);
    let table = hosting.table();
    let host = NonNull::from(&table).cast::<c_void>();
    let given = blaze_ladder::run::Given { host: Some(host), args: Some(argv.c()) };
    // Handed a Rust program with std, the runner refuses it by name (`RunError::OwnProcess`),
    // an error here like any other: never a second route.
    let ran = blaze_ladder::run::run_program_given(files, program_root, given, new, shared);
    // The table points into the host, `given.args` into `argv`: all are dropped only after the
    // program returned.
    drop(table);
    drop(hosting);
    drop(argv);
    let ran = ran.map_err(CliError::Program)?;
    // The run's closing note, on the error stream (standard output is the program's alone): a
    // reached function that is not built is never passed over in silence, called or not.
    if ran.not_built > 0 {
        launch.io.need_err()?.line(cx, format_args!("note: {} of {} reachable functions are not built yet; none was called", ran.not_built, ran.reached))?;
    }
    Ok(ran.status)
}

/// The exit status of `blaze run` and `blaze test` for every failure before the program starts
/// (the manifest, a finding, the build, the start; main.rs `main` and `close`), and when blaze
/// stopped it at a function that is not built ([`CliError::Ran`]): the tool's own failure (D86,
/// the ladder's L6), as `env`, `timeout` and `xargs` say it. The program's own status passes
/// through unchanged, and 101 stays a Rust program's panic.
pub(crate) const NOT_RUN: u8 = 125;

/// The `dev` profile's `overflow-checks` (decision D101), which `blaze run` builds the program
/// with and `blaze check` finds what that build refuses with: on, unless the workspace root's
/// manifest (`Cargo.toml` at `root`, the checkout's root, where the layout reads the workspace
/// from; a member's own `[profile]` is not read, as cargo reads only the root's) says otherwise under `[profile.dev]` (blaze-manifest `layout::profile`).
/// `blaze export` will take `BuiltinProfile::Release` the same way (off unless set).
pub fn dev_overflow(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<OverflowChecks, CliError> {
    let refused = |why: &dyn core::fmt::Display| CliError::Program(RunError::Manifest { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) });
    let mut bytes = Bytes::new_in(frame, "the root manifest");
    root.read_all(cx, &Path::new(ROOT_MANIFEST)?, &mut bytes)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "the root manifest is not UTF-8" })?;
    let erased = blaze_own::Erased::new(frame);
    let on = profile::overflow_checks(text, ManifestId(0), ROOT_MANIFEST, BuiltinProfile::Dev, erased.handle()).map_err(|e| refused(&e))?;
    Ok(if on { OverflowChecks::On } else { OverflowChecks::Off })
}

/// The `dev` profile's `panic` (UNWIND.md 3.1), the program's profile fact, from which every
/// unit's strategy is derived (blaze-world `panic::unit_panic`): `unwind` unless the workspace
/// root's manifest says `[profile.dev] panic = "abort"` (Rust's rule: no `panic` key unwinds;
/// blaze-manifest `layout::panic`, which refuses any other value naming the manifest, the
/// profile and the value). Not read for a test build (blaze-world `panic::TEST_PROFILE`).
pub fn dev_profile_panic(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<panic::ProfilePanic, CliError> {
    let refused = |why: &dyn core::fmt::Display| CliError::Program(RunError::Manifest { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) });
    let mut bytes = Bytes::new_in(frame, "the root manifest");
    root.read_all(cx, &Path::new(ROOT_MANIFEST)?, &mut bytes)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "the root manifest is not UTF-8" })?;
    let erased = blaze_own::Erased::new(frame);
    panic::panic_strategy(text, ManifestId(0), ROOT_MANIFEST, BuiltinProfile::Dev, erased.handle()).map_err(|e| refused(&e))
}

/// The program's strategy for the record run of `blaze run` (`own_process::run_own`'s caller
/// passes it): the target-side program unit's, a binary, by blaze-world's one function.
pub fn dev_panic(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<blaze_ladder::run::Panic, CliError> {
    Ok(blaze_ladder::panic_strategy::of_profile(dev_profile_panic(cx, root, frame)?))
}

/// The workspace root's manifest, under the checkout's root.
const ROOT_MANIFEST: &str = "Cargo.toml";
