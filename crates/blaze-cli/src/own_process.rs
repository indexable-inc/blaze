//! `blaze run` for every program (run.md piece 6 and "The cut to the lazy parent (D124)";
//! bottom.md section 11; decision D103): it runs in its own process, never inside blaze's: its
//! `process::exit` would end blaze, and its `argv` would be blaze's own.
//!
//! The steps, each a failure before the program's first instruction that ends `blaze run` with
//! `program::NOT_RUN` (125, main.rs) and names the step ([`OwnError`]):
//!
//! 1. the records folder under the home (`<HOME>/` [`crate::run_parent::RECORDS`]) and the
//!    lazy parent over the home's one store ([`crate::run_parent::Lazy`] over
//!    [`crate::code_store::HomeCodeStore`]): NOTHING is built yet;
//! 2. today's start list placed and the entry line ([`crate::run_parent::ahead_of`]): the list's
//!    under today's certificate, else the roots' (the one front-end piece made before the start);
//! 3. the launcher found beside this binary ([`LAUNCHER`]);
//! 4. the launcher started with the program's `argv[0]` (the binary target's name), the words
//!    after `--`, the command's own environment plus the run mode's variables ([`start_env`]:
//!    the entry line, and the grants for a hosted program), the command's working directory,
//!    and the command's own terminal as its standard input, output and error;
//! 5. then the parent's loop ([`crate::run_loop::run_program`]), under D103's signal rules,
//!    each first call answered by one step of the record walk, until the program ends: its
//!    status is `blaze run`'s ([`crate::run_parent::status_of`]).
//!
//! THE START LIST (start_list.rs; run.md stage 7, D124). Read before the spawn under the list's
//! name for this (program, request shape). Written under today's certificate, its rows whose
//! record file is there are placed as they are right after the start, before any need is read
//! ([`crate::run_parent::Needs::place_ahead`]); under another, it is its symbols only, and each
//! is resolved at its first call like any other (old code is never published). The parent
//! records its first answers as the program runs, and at exit the list of them is written
//! under today's certificate with today's entry line, unless it equals the one held. A list
//! that does not read (an older form, torn bytes) is said on the error stream, the run starts
//! as with none, and today's list replaces it at exit; a list that could not be written is said
//! there too, after the program's end, and the program's status stands (the run is over; the
//! list is a hint).
//!
//! THE ONE WAIT WITHOUT A DEADLINE (D103, rule 18's named exemption): the program's lifetime
//! is the user's (a server runs for days), so the loop's run has no deadline of its own
//! ([`crate::run_loop::Bounds::run`]); each of its waits is still one tick long, and every
//! step of the parent's own work keeps its bound (the start, a write of answers, the grace
//! after a forwarded SIGTERM). The span it runs in says so: [`PROGRAM_SPAN`].

use core::fmt;

use blaze_launch::grants::{self, GrantsError, MOST_BYTES};
use blaze_ladder::code_records::Builder;
use blaze_ladder::run::special::EntryForm;
use blaze_own::Vec;
use bstd::Cx;
use bstd::env::VarName;
use bstd::io::Missing;
use bstd::mem::Frame;
use bstd::path::Path;

use crate::code_store::HomeCodeStore;
use crate::host_grants::{HostGrantError, host_grant};
use crate::program::Launch;
use crate::run_bytes::{Chosen, Delivery};
use crate::run_loop::{self, LoopError};
use crate::run_parent::{self, Front, Lazy, Needs, ParentError, RECORDS, RecordsDir};
use crate::start_list;

/// Sans-IO decision for whether a finished `blaze run` should unlink its `.telem` and `.meta`
/// files immediately on child exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunCleanup {
    UnlinkTelemAndMeta,
    KeepForViewer,
}

fn decide_run_cleanup(open: blaze_run::viewer::OpenExplore, viewer_active: bool) -> RunCleanup {
    match (open, viewer_active) {
        (blaze_run::viewer::OpenExplore::No, false) => RunCleanup::UnlinkTelemAndMeta,
        (blaze_run::viewer::OpenExplore::Open, _) | (_, true) => RunCleanup::KeepForViewer,
    }
}

/// The span the parent's loop runs in: the wait for the program's end, which D103 exempts from
/// a deadline (the module's doc says why).
pub(crate) const PROGRAM_SPAN: &str = "blaze run: the program to exit (no deadline: the program's lifetime is the user's, D103)";

/// The launcher's file name, beside this binary (crates/blaze-launcher, `[[bin]] name`).
pub(crate) const LAUNCHER: &str = "blaze-launcher";

/// The program's working directory: the command's own (`.` as the start resolves it in this
/// process), as `cargo run` and every shell give a program.
const WORKING_DIR: &str = ".";

/// Why `blaze run` could not run the program in a process of its own. Every one comes before
/// the program's first instruction, or is the parent's own failure while serving it.
#[derive(Debug)]
pub enum OwnError {
    /// The records, the records folder, or a first call.
    Parent(ParentError),
    /// Starting it, serving it, or the parent's signals (D103).
    Loop(LoopError),
    /// The command's world lacks what a run needs (`env`, `fs`, `host`, `exe`, `spawner`).
    Missing(Missing),
    /// This binary's own path has no folder to find the launcher in.
    NoLauncherFolder { exe: std::boxed::Box<str, std::alloc::Global> },
    /// The launcher at `launcher` (beside `exe`) could not be started; `there` says whether a
    /// file is at that path, looked at once the start failed ([`Beside`]).
    NotStarted { launcher: std::boxed::Box<str, std::alloc::Global>, exe: std::boxed::Box<str, std::alloc::Global>, there: Beside, error: bstd::procs::RunError },
    /// `count` `--grant` given to a program whose entry is `form`, not bstd's hosted entry:
    /// only a hosted program is handed grants (its host table); refused, never dropped.
    GrantsNotHosted { count: usize, form: EntryForm },
    /// A `--grant` a hosted program cannot be given (host_grants.rs).
    HostGrant(HostGrantError),
    /// The grants do not fit `BLAZE_LAUNCH_GRANTS` (blaze-launch grants.rs).
    Grants(GrantsError),
    /// The start list could not be read from the store (the store's own refusal).
    StartList(crate::start_list::ListError),
    /// `--open` was passed, but live telemetry is unavailable for this run.
    OpenWithoutTelemetry { why: &'static str },
    /// A line on the command's error stream.
    Out(bstd::out::OutError),
}

impl fmt::Display for OwnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "`blaze run` did not start the program (run.md piece 6)";
        match self {
            OwnError::Parent(error) => write!(f, "{error}"),
            OwnError::Loop(error) => write!(f, "{error}"),
            OwnError::Missing(missing) => write!(f, "{rule}: {missing}"),
            OwnError::NoLauncherFolder { exe } => write!(f, "{rule}: this binary's path `{exe}` has no folder, and `{LAUNCHER}` is looked for beside it"),
            OwnError::NotStarted { launcher, exe, there, error } => match there {
                Beside::Absent => write!(f, "{rule}: no `{LAUNCHER}` beside `{exe}` (looked for at `{launcher}`): it is built with blaze and installed next to it ({error})"),
                Beside::Present => write!(f, "{rule}: the launcher `{launcher}` is there and did not start: {error}"),
                Beside::NotLooked(why) => write!(f, "{rule}: the launcher `{launcher}` did not start: {error}; whether it is there could not be looked at: {why}"),
            },
            OwnError::GrantsNotHosted { count, form } => write!(
                f,
                "{rule}: {count} `--grant` given, and the program's entry is `{}`, not bstd's hosted entry: only a hosted program is handed grants (run.md, \"What a program is given\")",
                form.word()
            ),
            OwnError::HostGrant(error) => write!(f, "{rule}: {error}"),
            OwnError::Grants(error) => write!(f, "{rule}: {error}"),
            OwnError::StartList(error) => write!(f, "`blaze run` did not start the program (run.md stage 7): {error}"),
            OwnError::OpenWithoutTelemetry { why } => write!(f, "{rule}: `--open` needs live telemetry, and {why}"),
            OwnError::Out(error) => write!(f, "`blaze run`: the error stream: {error}"),
        }
    }
}

impl core::error::Error for OwnError {}

/// Whether a file is at the launcher's path, looked at only after its start failed (one stat,
/// on the error's path alone: a start that works costs nothing more).
#[derive(Debug)]
pub enum Beside {
    Absent,
    Present,
    /// The folder or the stat was refused: the refusal's words.
    NotLooked(std::boxed::Box<str, std::alloc::Global>),
}

/// Looks at `launcher` (an absolute path, [`launcher_beside`]): its folder is opened as a tree
/// of its own (bstd `Fs::home` over the folder's parent, the folder's name beneath it) and the
/// file's name is stat'ed there.
fn beside(cx: &Cx, fs: &bstd::fs::Fs, launcher: &str, frame: &Frame) -> Beside {
    let not_looked = |why: &dyn fmt::Display| Beside::NotLooked(std::boxed::Box::from(bstd::mem::format_in!(frame, "{why}").as_str()));
    let Some((folder, file)) = launcher.rsplit_once('/') else { return not_looked(&"the path has no folder") };
    let Some((parent, name)) = folder.rsplit_once('/') else { return not_looked(&"the folder is not an absolute path") };
    let parent = if parent.is_empty() { "/" } else { parent };
    let (name, file) = match (Path::new(name), Path::new(file)) {
        (Ok(name), Ok(file)) => (name, file),
        (Err(why), _) | (_, Err(why)) => return not_looked(&why),
    };
    let dir = match fs.home(cx, parent, &name) {
        Ok(dir) => dir,
        Err(why) => return not_looked(&why),
    };
    match dir.stat(cx, &file) {
        Ok(Some(_)) => Beside::Present,
        Ok(None) => Beside::Absent,
        Err(why) => not_looked(&why),
    }
}

/// The launcher's path: [`LAUNCHER`] in the folder of `exe`, this binary's own path.
pub(crate) fn launcher_beside<'f>(exe: &str, frame: &'f Frame) -> Result<bstd::mem::String<&'f Frame>, OwnError> {
    let Some((folder, _)) = exe.rsplit_once('/') else {
        return Err(OwnError::NoLauncherFolder { exe: std::boxed::Box::from(exe) });
    };
    Ok(bstd::mem::format_in!(frame, "{folder}/{LAUNCHER}"))
}

/// Runs the root package `package` in a process of its own under the lazy parent: the
/// program's status. See the module's doc. `builder` is what its records are made for
/// (run_parent.rs `builder_for`: the edition, the profile's overflow checks and panic strategy,
/// the code builder's identity). `front` makes the front end on demand (run_parent.rs
/// `CommandFront`: the names records and the body ids, each made on the first need of it).
/// `lent` is the command's one store (main.rs opens it before the request and commits it after
/// the answer): the program's code entries are found in it and put into it
/// ([`crate::code_store::HomeCodeStore`]), so the next `blaze run` finds what this one made.
/// `today` is the request's certificate and the start list's name (main.rs's
/// `Effect::StartProgram` site): the list is read under it before the start and written under
/// it at exit (the module's doc). `native` is the program's native library when its image holds
/// native code (engine_native.rs `NativeSend`): named in the child's start
/// (`BLAZE_LAUNCH_NATIVE`) and told as the first frame, before today's list and the
/// personality (blaze-launch wire/run.rs `Down::Native`); `None` sends neither.
#[expect(clippy::too_many_arguments, reason = "the own-process run's edge: each argument one input, handle or allocator of it")]
pub(crate) fn run_own<'h, F: Front<Maker = crate::run_parent::RunMaker<'h>>>(
    cx: &Cx,
    launch: &Launch<'_>,
    package: &str,
    builder: Builder,
    front: F,
    native: Option<crate::engine_native::NativeSend<'_>>,
    lent: &crate::store::CommandLent,
    today: start_list::Today,
    // The run's closing count, for the profile's `run` line on every run (phases.rs): set once
    // the program's loop ends, whether it ended well or not.
    counted: &mut Option<crate::run_parent::RunTally>,
    frame: &Frame,
) -> Result<u8, OwnError> {
    let env = launch.env.ok_or(OwnError::Missing(bstd::io::Missing { capability: "env" }))?;
    let fs = launch.fs;
    let host = launch.host;
    let err = launch.err;
    // 1. The records folder, under the home the command was started with (rule 75: read here,
    // at the edge, from the command's own environment), and the lazy parent: nothing built.
    // The folder is the launcher's `BLAZE_LAUNCH_STORE` either way (the program's native
    // library is a file there, engine_native.rs).
    let home_name = VarName::new("HOME").map_err(|_| OwnError::Parent(ParentError::NoHome))?;
    let home = env.get(&home_name).ok().flatten().ok_or(OwnError::Parent(ParentError::NoHome))?;
    let records = Path::new(RECORDS).map_err(|why| OwnError::Parent(ParentError::Path(why)))?;
    let folder = fs.home(cx, home, &records).map_err(|why| OwnError::Parent(ParentError::Home(why)))?;
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let toolchain = builder.toolchain;
    let mut codes = HomeCodeStore::new(lent);
    // THE SEAM LINE: how the records reach the program (run_bytes.rs `Delivery`; `Inline` sends
    // them over descriptor 3 and files none).
    let mut lazy = Lazy::new(cx, front, Chosen::of(Delivery::Inline, RecordsDir { cx, dir: &folder }, alloc), &mut codes, builder, alloc);
    let store = bstd::mem::format_in!(frame, "{}/{RECORDS}", home.trim_end_matches('/'));
    // 2. The start list held for this (program, request shape), read before the spawn; its
    // rows placed, and the entry line.
    let held = start_list::held(lent, &today.name).map_err(OwnError::StartList)?;
    // What the list was, for the profile's start-list line (an older form is said there too).
    let mut listed = run_parent::ListHeld::None;
    let list = match held.map(|bytes| start_list::read(bytes, alloc)) {
        None => None,
        Some(Ok(view)) => Some(view.under(&today.certificate)),
        Some(Err(why)) => {
            listed = run_parent::ListHeld::Unread(why);
            run_parent::Resolve::watch(&lazy).see(blaze_screen::Step::Says);
            err.line(cx, format_args!("blaze run: {why}; the program starts without it, and this run's list replaces it at exit")).map_err(OwnError::Out)?;
            None
        }
    };
    let rows = match list {
        Some(start_list::Held::Today(rows)) => {
            listed = run_parent::ListHeld::Today;
            Some(rows)
        }
        // Another certificate's symbols: each is resolved at its first call (compiled-behind,
        // run.md piece 10, will resolve them ahead on idle workers).
        Some(start_list::Held::Other(_)) => {
            listed = run_parent::ListHeld::Other;
            None
        }
        None => None,
    };
    let mut needs = Needs::new(alloc);
    let mut ahead = std::vec::Vec::new_in(alloc);
    let mut placed = Vec::new_in(alloc);
    // The native library first: before any `Declare` (the child hands the entry over only once
    // it has loaded it).
    if let Some(send) = native {
        needs.tell_native(send.library, send.words, &mut ahead).map_err(OwnError::Parent)?;
    }
    let head = run_parent::ahead_of(rows, &mut lazy, &mut needs, &mut placed, &mut ahead).map_err(OwnError::Parent)?;
    let line = head.line;
    let entry = bstd::mem::format_in!(frame, "{line}");
    // 3. The launcher.
    let exe = host.exe().map_err(OwnError::Missing)?;
    let launcher = launcher_beside(&exe, frame)?;
    // 4. Its environment: the command's own, then the run mode's.
    let mut granted = [0u8; MOST_BYTES];
    let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    start_env(env.each(), store.as_str(), entry.as_str(), line.form, launch.grants, &mut granted, frame, &mut vars)?;
    // `launch_env` left out any `BLAZE_LAUNCH_NATIVE` of the command's own: the parent's word.
    if let Some(send) = native {
        vars.push(("BLAZE_LAUNCH_NATIVE", send.hex));
    }
    // Live shared-memory telemetry ring (non-comptime, opt-out via `BLAZE_NO_TELEMETRY=1`);
    // opening the Explore web page is opt-in via `blaze run --open`.
    let no_telemetry = VarName::new("BLAZE_NO_TELEMETRY")
        .ok()
        .and_then(|name| env.get(&name).ok().flatten())
        .is_some();
    let runs_dir = Path::new(".blaze/v2/runs").ok().and_then(|rel| fs.home(cx, home, &rel).ok());
    let run_stem = if line.form != EntryForm::Comptime && !no_telemetry {
        let cert = today.certificate.prefix();
        Some(bstd::mem::format_in!(
            frame,
            "{}-{:02x}{:02x}{:02x}{:02x}",
            package,
            cert[0],
            cert[1],
            cert[2],
            cert[3],
        ))
    } else {
        None
    };
    let telem_path = run_stem.as_ref().map(|stem| {
        bstd::mem::format_in!(frame, "{}/.blaze/v2/runs/{}.telem", home.trim_end_matches('/'), stem.as_str())
    });
    if let (Some(dir), Some(stem)) = (runs_dir.as_ref(), run_stem.as_ref()) {
        let reason = match listed {
            run_parent::ListHeld::Today => "Certificate cache hit (start list pre-placed)",
            run_parent::ListHeld::Other => "Certificate changed (source or dependency edit invalidated start list)",
            run_parent::ListHeld::None => "Cold first build (no prior start list for this target)",
            run_parent::ListHeld::Unread(_) => "Prior start list unreadable (rebuilding fresh list)",
        };
        let body = bstd::mem::format_in!(
            frame,
            "{{\"id\":\"{}\",\"package\":\"{}\",\"command\":\"blaze run\",\"status\":\"running\",\"exit_code\":null,\"errors\":0,\"warnings\":0,\"reached\":{},\"found\":{},\"made\":0,\"placed\":{},\"resolved\":0,\"frontend_built\":\"pending\",\"recompile_reason\":\"{}\",\"phases\":[]}}\n",
            stem.as_str(),
            package,
            placed.len(),
            placed.len(),
            placed.len(),
            reason,
        );
        let name = bstd::mem::format_in!(frame, "{}.build.json", stem.as_str());
        if let Ok(rel) = Path::new(name.as_str()) {
            let _ = dir.publish(cx, &rel, bstd::fs::Content::bytes(body.as_bytes()), bstd::fs::Durable::No);
        }
    }
    let telemetry = match (line.form, no_telemetry, telem_path.as_ref()) {
        (_, _, Some(path)) => Telemetry::On(path.as_str()),
        (EntryForm::Comptime, _, None) => Telemetry::Comptime,
        (_, _, None) => Telemetry::DisabledByEnv,
    };
    explore_env(launch.open, telemetry, &mut vars)?;
    let spawner = host.spawner().map_err(OwnError::Missing)?;
    // The program's command line, for every entry form: its name first (`arg0`, the binary
    // target's), then the words after `--`.
    let started = run_loop::Launch { launcher: launcher.as_str(), arg0: package, args: launch.args, env: &vars, dir: WORKING_DIR };
    // 5. The loop, in the span that names its one exempt wait; today's list placed first.
    let ran = {
        let cx = cx.span(PROGRAM_SPAN);
        run_loop::run_program(&cx, &spawner, &started, &mut needs, &mut lazy, &ahead, alloc)
    };
    let tally = lazy.tally();
    let start_tally = needs.start_tally(lazy.built(), listed);
    *counted = Some(run_parent::RunTally { codes: tally, start: start_tally, lowerings: lazy.lowering_counts(), walk: lazy.walk_counts() });
    if let (Some(dir), Some(stem)) = (runs_dir.as_ref(), run_stem.as_ref()) {
        let exit_code = ran.as_ref().ok().map(|r| r.status);
        let status_str = match exit_code {
            Some(0) => "ok",
            Some(_) | None => "failed",
        };
        let exit_json = match exit_code {
            Some(code) => bstd::mem::format_in!(frame, "{code}"),
            None => bstd::mem::format_in!(frame, "null"),
        };
        let (fe_built, reason) = match (start_tally.held, start_tally.built) {
            (run_parent::ListHeld::Today, run_parent::Built::No) => (
                "no",
                "100% certificate cache hit — zero front-end work, all functions served from cache",
            ),
            (run_parent::ListHeld::Today, run_parent::Built::Need { .. }) => (
                "lazy_need",
                "Start list hit, but a newly reached code path triggered lazy front-end resolution",
            ),
            (run_parent::ListHeld::Other, _) => (
                "head",
                "Certificate changed (source or dependency edit) — re-resolved changed functions",
            ),
            (run_parent::ListHeld::None, _) => (
                "head",
                "Cold first run — compiled reached functions and recorded start list for next run",
            ),
            (run_parent::ListHeld::Unread(_), _) => (
                "head",
                "Prior start list schema changed — rebuilt fresh start list",
            ),
            (run_parent::ListHeld::Today, run_parent::Built::Head) => (
                "head",
                "Rebuilt start head under today's certificate",
            ),
        };
        let body = bstd::mem::format_in!(
            frame,
            "{{\"id\":\"{}\",\"package\":\"{}\",\"command\":\"blaze run\",\"status\":\"{}\",\"exit_code\":{},\"errors\":{},\"warnings\":0,\"reached\":{},\"found\":{},\"made\":{},\"placed\":{},\"resolved\":{},\"frontend_built\":\"{}\",\"recompile_reason\":\"{}\",\"phases\":[]}}\n",
            stem.as_str(),
            package,
            status_str,
            exit_json.as_str(),
            if exit_code == Some(0) { 0u32 } else { 1u32 },
            tally.reached,
            tally.found,
            tally.made,
            start_tally.placed,
            start_tally.resolved,
            fe_built,
            reason,
        );
        let name = bstd::mem::format_in!(frame, "{}.build.json", stem.as_str());
        if let Ok(rel) = Path::new(name.as_str()) {
            let _ = dir.publish(cx, &rel, bstd::fs::Content::bytes(body.as_bytes()), bstd::fs::Durable::No);
        }
        // Reclaim the 256 KiB `.telem` ring and `.meta` immediately on child exit when `--open`
        // was not passed and no live viewer holds `v2/runs/viewer.lock` (1 stat fast path when
        // no viewer has run; leaves files for the viewer's 15s collector when one is active).
        let viewer_active = match launch.open {
            blaze_run::viewer::OpenExplore::Open => true,
            blaze_run::viewer::OpenExplore::No => {
                Path::new(".blaze")
                    .ok()
                    .and_then(|rel| fs.home(cx, home, &rel).ok())
                    .map_or(false, |blaze_dir| crate::gc::viewer_lock_is_held(cx, &blaze_dir))
            }
        };
        if decide_run_cleanup(launch.open, viewer_active) == RunCleanup::UnlinkTelemAndMeta {
            for ext in [".telem", ".meta"] {
                let leaf = bstd::mem::format_in!(frame, "{}{ext}", stem.as_str());
                if let Ok(rel) = Path::new(leaf.as_str()) {
                    let _ = dir.remove(cx, &rel, bstd::fs::Durable::No);
                }
            }
        }
    }
    // A crash the program's handler told by place, named while the walk is alive (rule 19,
    // run_fault.rs): the fn by its path, the trap by the instruction in its record.
    let mut struck_in = bstd::mem::Bytes::new_in(frame, "the code record a crash struck in");
    let crash = needs.fault().map(|fault| {
        let record = match fault.at {
            Some(spot) if spot.placed == blaze_launch::wire::fault::Placed::Code => Some(lazy.read_record(&spot.record, &mut struck_in).map(|()| struck_in.as_slice())),
            _ => None,
        };
        crate::run_fault::report(fault, &mut lazy, record)
    });
    // The walk is over: what the code store served and made is final (the program root's half).
    drop(lazy);
    let walked = codes.reached();
    let toolchain_made = codes.toolchain_made();
    let ran = ran.map_err(|error| match error {
        LoopError::Start(error) => OwnError::NotStarted {
            launcher: std::boxed::Box::from(launcher.as_str()),
            exe: std::boxed::Box::from(&*exe),
            there: beside(cx, &fs, launcher.as_str(), frame),
            error,
        },
        other => OwnError::Loop(other),
    })?;
    if let Some(crash) = &crash {
        err.line(cx, format_args!("{crash}")).map_err(OwnError::Out)?;
    }
    // A clean run stays quiet on stderr so its output is the program's alone (`run_output.rs`);
    // the profile always records `{tally}`.
    let _ = tally;
    // This run's first answers, the next start's list, under today's certificate.
    let mut bytes = blaze_own::Vec::new_in(alloc);
    let wrote = needs
        .first()
        .encode(&today.certificate, head, &mut bytes)
        .map_err(start_list::ListError::Cap)
        .and_then(|()| start_list::write(lent, &today.name, held, bytes.as_slice()));
    let list = match wrote {
        Ok(_) => Some(bytes.as_slice()),
        Err(why) => {
            err.line(cx, format_args!("blaze run: the program ended with {}, and its start list was not kept: {why}", ran.status)).map_err(OwnError::Out)?;
            None
        }
    };
    // The program's code and its list, named by the program root (per program, request shape
    // and user, store/roots.rs) for the next run from any checkout: the start list's keys and
    // the walk's, one index, one root.
    match crate::store::roots::keep_program(lent, &today, list, &walked) {
        Ok((_rooted, kept)) if kept.stale_rows > 0 => {
            err.line(cx, format_args!("blaze run: {} start-list rows name code the store no longer holds; the next run builds them again", kept.stale_rows)).map_err(OwnError::Out)?;
        }
        Ok(_) => {}
        Err(why) => {
            err.line(cx, format_args!("blaze run: the program ended with {}, and its code was not kept for the next run: {why}", ran.status)).map_err(OwnError::Out)?;
        }
    }
    // The toolchain's code this run made, and the toolchain's root renewed: std's compiled code
    // outlives this program's root, for every project on this toolchain.
    if let Err(why) = crate::store::roots::keep_toolchain(lent, toolchain, &toolchain_made, &[]) {
        err.line(cx, format_args!("blaze run: the program ended with {}, and the toolchain's code was not kept: {why}", ran.status)).map_err(OwnError::Out)?;
    }
    Ok(ran.status)
}

/// Whether live telemetry is enabled for this run: disabled by `BLAZE_NO_TELEMETRY` or on a
/// compile-time entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Telemetry<'a> {
    On(&'a str),
    Comptime,
    DisabledByEnv,
}

/// Pushes the launcher's telemetry and Explore variables onto `vars`:
/// - `BLAZE_LAUNCH_TELEMETRY` whenever telemetry is on;
/// - `BLAZE_LAUNCH_EXPLORE=1` ONLY when `--open` was passed (`OpenExplore::Open`).
/// Refuses `--open` when telemetry is unavailable so the flag is never quietly ignored (rule 3).
fn explore_env<'e>(
    open: blaze_run::viewer::OpenExplore,
    telemetry: Telemetry<'e>,
    vars: &mut Vec<(&'e str, &'e str), &Frame>,
) -> Result<(), OwnError> {
    match (open, telemetry) {
        (_, Telemetry::On(path)) => {
            vars.push((blaze_launch::telemetry::VAR, path));
            if open == blaze_run::viewer::OpenExplore::Open {
                vars.push(("BLAZE_LAUNCH_EXPLORE", "1"));
            }
            Ok(())
        }
        (blaze_run::viewer::OpenExplore::No, Telemetry::Comptime | Telemetry::DisabledByEnv) => Ok(()),
        (blaze_run::viewer::OpenExplore::Open, Telemetry::Comptime) => {
            Err(OwnError::OpenWithoutTelemetry { why: "a compile-time entry emits no telemetry" })
        }
        (blaze_run::viewer::OpenExplore::Open, Telemetry::DisabledByEnv) => {
            Err(OwnError::OpenWithoutTelemetry { why: "`BLAZE_NO_TELEMETRY` turned live telemetry off" })
        }
    }
}

/// The launcher run mode's environment for the entry line `entry` (`<form>:<64 hex>`, its form
/// `form`) and the command line's grants: the ONE place both are put. A HOSTED program is given
/// `BLAZE_LAUNCH_GRANTS`, even when empty: each `--grant` read by host_grants.rs `host_grant`,
/// written by blaze-launch grants.rs `format` into `granted`. Any other form is given no grants
/// variable, and a `--grant` for it is refused by count, never dropped.
#[expect(clippy::too_many_arguments, reason = "the environment's one home: each argument one part of it")]
fn start_env<'e>(
    env: impl Iterator<Item = (&'e str, &'e str)>,
    store: &'e str,
    entry: &'e str,
    form: EntryForm,
    grants: &'e [(&'e str, &'e str)],
    granted: &'e mut [u8; MOST_BYTES],
    frame: &Frame,
    out: &mut Vec<(&'e str, &'e str), &Frame>,
) -> Result<(), OwnError> {
    let value = match form {
        EntryForm::Hosted => {
            let mut pairs: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(grants.len(), frame);
            for (at, (name, value)) in grants.iter().enumerate() {
                pairs.push(host_grant(at + 1, name, value).map_err(OwnError::HostGrant)?);
            }
            Some(&*grants::format(&pairs, granted).map_err(OwnError::Grants)?)
        }
        EntryForm::Unit | EntryForm::CMain | EntryForm::Comptime => {
            if !grants.is_empty() {
                return Err(OwnError::GrantsNotHosted { count: grants.len(), form });
            }
            None
        }
    };
    run_parent::launch_env(env, store, entry, value, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts: the launcher is found beside the binary; a hosted program gets
    /// `BLAZE_LAUNCH_GRANTS` (empty with no grant, the pair with one); a `c-main` program gets
    /// no grants variable, and a `--grant` for it is refused by count; the entry line is set
    /// once, over a command variable of the same name.
    /// Prints "own process: the launcher beside the binary; grants for a hosted entry alone".
    #[test]
    fn the_launcher_is_beside_and_grants_go_to_a_hosted_entry_alone() {
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        assert_eq!(launcher_beside("/opt/blaze/bin/blaze", &frame).expect("a folder").as_str(), "/opt/blaze/bin/blaze-launcher");
        assert!(matches!(launcher_beside("blaze", &frame), Err(OwnError::NoLauncherFolder { .. })));
        let command = [("PATH", "/bin"), ("BLAZE_LAUNCH_ENTRY", "theirs")];
        let var = |vars: &[(&str, &str)], name: &str| vars.iter().filter(|(n, _)| *n == name).map(|(_, v)| std::string::String::from(*v)).collect::<std::vec::Vec<_>>();

        let mut granted = [0u8; MOST_BYTES];
        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        start_env(command.iter().copied(), "/h/records", "hosted:00", EntryForm::Hosted, &[], &mut granted, &frame, &mut vars).expect("a hosted start");
        assert_eq!(var(&vars, grants::VAR), [""], "a hosted program with no grant is not told so");
        assert_eq!(var(&vars, "BLAZE_LAUNCH_ENTRY"), ["hosted:00"], "the command's own entry variable was kept");

        let mut granted = [0u8; MOST_BYTES];
        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        let given = [("out", "whole")];
        start_env(command.iter().copied(), "/h/records", "hosted:00", EntryForm::Hosted, &given, &mut granted, &frame, &mut vars).expect("a hosted start");
        assert_eq!(var(&vars, grants::VAR).len(), 1);
        assert!(var(&vars, grants::VAR)[0].contains("out"), "the grant's name is not in the value: {:?}", var(&vars, grants::VAR));

        let mut granted = [0u8; MOST_BYTES];
        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        start_env(command.iter().copied(), "/h/records", "c-main:00", EntryForm::CMain, &[], &mut granted, &frame, &mut vars).expect("a c-main start");
        assert!(var(&vars, grants::VAR).is_empty(), "a c-main program was given a grants variable");
        let mut granted = [0u8; MOST_BYTES];
        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        let refused = start_env(command.iter().copied(), "/h/records", "c-main:00", EntryForm::CMain, &given, &mut granted, &frame, &mut vars);
        assert!(matches!(refused, Err(OwnError::GrantsNotHosted { count: 1, form: EntryForm::CMain })), "a grant for a c-main program was not refused");
        println!("own process: the launcher beside the binary; grants for a hosted entry alone");
    }

    /// Asserts: once a start fails, the launcher's path is looked at: with no file there the
    /// error says "no `blaze-launcher` beside `<exe>`" and names the path tried; with the file
    /// there it is `Present` (the control: a probe that always said absent fails it).
    /// Prints "own process: a launcher not beside blaze is named as missing".
    #[test]
    fn a_launcher_not_beside_blaze_is_named_as_missing() {
        use bstd::fs::{Content, Durable};
        use bstd::io::{Io, Limits, SimWorld};
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let fs = io.need_fs().expect("files");
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        let exe = "/opt/blaze/bin/blaze";
        let launcher = launcher_beside(exe, &frame).expect("a folder");
        let there = beside(&cx, &fs, launcher.as_str(), &frame);
        assert!(matches!(there, Beside::Absent), "no launcher was written, and the probe said {there:?}");
        let error = OwnError::NotStarted {
            launcher: std::boxed::Box::from(launcher.as_str()),
            exe: std::boxed::Box::from(exe),
            there,
            error: bstd::procs::RunError { step: bstd::procs::RunStep::Start, proc: None, fault: bstd::procs::RunFault::Host { what: "starting it", os: 0 } },
        };
        let said = std::format!("{error}");
        assert!(said.contains("no `blaze-launcher` beside `/opt/blaze/bin/blaze`") && said.contains("/opt/blaze/bin/blaze-launcher"), "{said}");
        fs.root().publish(&cx, &Path::new("opt/blaze/bin/blaze-launcher").expect("a path"), Content::bytes(b"launcher"), Durable::No).expect("write");
        let there = beside(&cx, &fs, launcher.as_str(), &frame);
        assert!(matches!(there, Beside::Present), "the launcher was written, and the probe said {there:?}");
        println!("own process: a launcher not beside blaze is named as missing");
    }

    /// Asserts: by default (`OpenExplore::No`), `BLAZE_LAUNCH_EXPLORE` is not set even when
    /// telemetry is on; `--open` (`OpenExplore::Open`) sets `BLAZE_LAUNCH_EXPLORE=1`; `--open`
    /// with telemetry disabled or on a comptime entry is refused by name instead of ignored.
    #[test]
    fn explore_opens_only_when_requested_and_refuses_without_telemetry() {
        use blaze_run::viewer::OpenExplore;
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        fn var<'a, 'f>(vars: &[(&str, &'a str)], name: &str, frame: &'f Frame) -> Vec<&'a str, &'f Frame> {
            let mut out = Vec::new_in(frame);
            for &(n, v) in vars {
                if n == name {
                    out.push(v);
                }
            }
            out
        }

        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        explore_env(OpenExplore::No, Telemetry::On("/h/.blaze/v2/runs/app.telem"), &mut vars).expect("default run");
        assert_eq!(var(&vars, blaze_launch::telemetry::VAR, &frame).as_slice(), ["/h/.blaze/v2/runs/app.telem"]);
        assert!(var(&vars, "BLAZE_LAUNCH_EXPLORE", &frame).is_empty(), "default `blaze run` must not set BLAZE_LAUNCH_EXPLORE");

        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        explore_env(OpenExplore::Open, Telemetry::On("/h/.blaze/v2/runs/app.telem"), &mut vars).expect("--open run");
        assert_eq!(var(&vars, blaze_launch::telemetry::VAR, &frame).as_slice(), ["/h/.blaze/v2/runs/app.telem"]);
        assert_eq!(var(&vars, "BLAZE_LAUNCH_EXPLORE", &frame).as_slice(), ["1"]);

        let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(&frame);
        assert!(matches!(
            explore_env(OpenExplore::Open, Telemetry::DisabledByEnv, &mut vars),
            Err(OwnError::OpenWithoutTelemetry { .. })
        ));
        assert!(matches!(
            explore_env(OpenExplore::Open, Telemetry::Comptime, &mut vars),
            Err(OwnError::OpenWithoutTelemetry { .. })
        ));
    }
}
