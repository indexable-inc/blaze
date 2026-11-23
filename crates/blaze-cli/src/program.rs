//! `blaze run`'s program, after its check: the package's source files read through bstd and
//! handed to the walking skeleton's straight line (`blaze_ladder::run::run_program`: the new
//! front end over the toolchain's shelf), which places the code in this process and calls
//! `main` (crates/blaze-cli/tests/ladder/README.md, route (e)). A reached function this build
//! cannot make is a stub: the program runs, and stops by name only if it calls one. Which
//! process the program runs in is decided first, before anything is built
//! (`blaze_ladder::run::entry_start`, the program's start as the launcher's form): every
//! program goes to its own process (own_process.rs; run.md "Every program runs in the child")
//! and is never built here.

use blaze_ladder::run::OverflowChecks;
use blaze_manifest::layout::panic;
use blaze_manifest::layout::profile::{self, BuiltinProfile};
use blaze_manifest::layout::ManifestId;
use bstd::Cx;
use bstd::env::Env;
use bstd::fs::{Dir, Fs, ReadWrite};
use bstd::host::Host;
use bstd::mem::{Bytes, Frame};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;

/// What `blaze run` hands the program: the specific capabilities the launcher uses (`fs`, `env`,
/// `host`, `out`, `err`), which program (`-p`, `--bin`: run_package.rs), whether `--open` asked
/// to open the Explore web page, the grants named on its command line (`--grant name=value`,
/// run.md), and the words after `--`.
pub struct Launch<'a> {
    pub fs: &'a Fs,
    pub env: Option<&'a Env>,
    pub host: &'a Host,
    pub out: &'a Out,
    pub err: &'a Out,
    pub select: crate::run_package::Select<'a>,
    pub open: blaze_run::viewer::OpenExplore,
    pub grants: &'a [(&'a str, &'a str)],
    pub args: &'a [&'a str],
}

/// Runs the package at `root`: the status `main` ended with. A hosted program (its entry bstd's
/// `bstd_hosted_start`) reaches the world only through the host table made here, over the
/// command's `Io` and only what `launch` grants; the host lives until the program has returned.
///
/// Its files are the root package's `src/` folder, listed: the program starts beside its check
/// (`Gate::Beside`), before the walk has opened a file, so the walk's rows are
/// not there yet on a first run. Only `src/` is listed, never the checkout: a `target/` of a
/// million entries beside it costs nothing (it filled the command's frame when the whole
/// checkout was listed). Owed (drive.md, "RUNNER 11"): the runner walks forward from the
/// program's root (run_package.rs `Program::root`) through a loader, so a file no `mod` reaches
/// is never read.
///
/// `picked` is the program `request` picked once (run_package.rs `pick`, main.rs): never picked
/// again here, so the manifests are read once a command (rule 77).
///
/// `units` is the key `Effect::StartProgram` carried: the units' record that names the entry,
/// taken here so the runner never reads it back from the request (today's straight line does
/// not read it). `native` is what the start is sent of the program's native image
/// (engine_native.rs [`crate::engine_native::NativeSend`]): `Some` exactly when the image holds
/// native code, and then the child loads it before `main` (blaze-launch wire/run.rs
/// `Down::Native`); a program whose image holds C never starts without it.
///
/// `producer` is the code builder's identity from the command's producer table (D105): the
/// record walk's code keys hold it.
#[expect(unused_variables, reason = "`units` waits for the image path that loads the program from its records")]
#[expect(clippy::too_many_arguments, reason = "the run's edge: each argument one handle or fact of it")]
pub(crate) fn run<'h>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    launch: &Launch<'_>,
    picked: &crate::run_package::Program<'_>,
    units: blaze_drive::Key,
    native: Option<crate::engine_native::NativeSend<'_>>,
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
    // What the run reads each body's checked columns from, besides the ids: the check's typed
    // records, by kind (engine_body_record.rs `TypedFrom`, run_parent.rs `RunMaker`).
    typed: crate::engine_body_record::TypedFrom<'h>,
    // The run's closing count, for the profile (own_process.rs `run_own`).
    counted: &mut Option<crate::run_parent::RunTally>,
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
    // The parent's failures boxed in the process heap: the error outlives this frame to `main`.
    let parent = move |why| CliError::OwnProcess(blaze_own::Box::new_in(crate::own_process::OwnError::Parent(why), heap.alloc()));
    let isa = crate::run_parent::isa().map_err(parent)?;
    let builder = crate::run_parent::builder_for(edition, overflow, panic, producer, isa).map_err(parent)?;
    // EVERY program runs in its own process under the lazy parent (own_process.rs, D103, D124;
    // run.md "Every program runs in the child"), and nothing of it is built here: its start
    // (the roots: bstd's hosted entry, a C `main`, the shim of a Rust program with std, or
    // `main` called directly) is read from the names records only when today's start list does
    // not give it, and each function is found or made at its first call. Its status is the
    // program's, unchanged; a failure of the parent's own is `CliError::OwnProcess`, which
    // `run` ends with `NOT_RUN` (main.rs).
    let roots = blaze_own::Erased::new(frame);
    // The Rust fns the program's C calls are roots of its walk (run_parent.rs `CommandFront`).
    let exports = native.map_or(&[][..], |send| send.items);
    // The make watch's clock starts here, at the command's edge (rule 75).
    let make_watch = crate::run_parent::command_make_watch();
    let front = crate::run_parent::CommandFront::new(names, ids, typed, builder, heap.alloc(), roots.handle(), watch, exports, make_watch);
    crate::own_process::run_own(&cx, launch, package, builder, front, native, lent, today, counted, frame).map_err(move |error| CliError::OwnProcess(blaze_own::Box::new_in(error, heap.alloc())))
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
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
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
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
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
