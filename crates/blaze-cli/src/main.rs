//! `blaze`, on the execution engine (blaze-engine): `blaze check [dir]` (check_command.rs over
//! engine_check.rs), `blaze run [dir]` and `blaze test [dir]` (engine_start.rs: the engine's
//! check, then the program or the tests over what it stored), `blaze clean`, `blaze explain`.
//! This binary performs every effect through bstd and nothing else. `[dir]` is the checkout,
//! relative to where blaze was started, in any spelling a person writes ([`checkout`]); `.` or no
//! argument is the start directory itself, bstd's `Fs::root`. The old request machine
//! (blaze-drive's `Request` and its effect loop here) is gone (2026-11-19).
//!
//! | module | what |
//! |---|---|
//! | [`engine_check`] | the check: the engine's stages, its answer and what it printed |
//! | [`engine_start`] | `run` and `test` from the engine's answer |
//! | [`store`] | records by key, behind the `Store` trait |
//! | [`print`] | findings drawn by blaze-report, the closing line, and the profile |
//!
//! After the command: `--color=auto|always|never` (auto: frames on a terminal or under
//! `CLICOLOR_FORCE`, lines otherwise; blaze_report::frame::detect), `--stats` (the profile's
//! summary line on stderr), `--origin <folder> --origin-key <hex>` (the shared cache).

#![feature(macro_attr, allocator_api, alloc_error_hook)]
#![forbid(unsafe_code)]

mod body_types;
// The code index over the home's store: run_parent.rs `Lazy` finds and keeps a program's code
// entries there across commands.
mod code_store;
mod item_name;
mod checkout;
mod clean;
mod data_include;
mod fetch;
mod fix;
mod front_load;
mod gc;
mod home;
mod job_texts;
mod lints;
mod macro_files;
mod macros;
mod proc_callers;
mod proc_env;
#[cfg(test)]
mod proc_env_pkg_meta_tests;
#[cfg(test)]
mod proc_env_unknown_tests;
mod proc_runner;
mod names;
mod native_image;
mod native_runner_c;
mod script_modules;
mod scripts;
mod sdk;
mod not_checked;
mod peer;
mod phases;
mod print;
mod producers;
mod program;
// `--grant <parameter>=<scope>` as bstd's host's `(kind, name)` (run.md; D104 lane's finding).
mod host_grants;
// `blaze run` of a Rust-edition program: in its own process, under the run parent (D103).
mod own_process;
// The run parent's sans-IO half; its driver loop waits on the bstd surface its head names.
#[allow(dead_code, reason = "called once the driver loop lands (run_parent.rs head, NOT HERE 1 to 3)")]
mod run_parent;
// A run's records sent inline over descriptor 3 (run wire `Bytes`), chosen at each caller's seam
// line beside the records folder (files stay the default until its tests pass).
mod run_bytes;
// Every folder blaze writes under a home's and a checkout's `.blaze`, and which clean removes it.
mod layout;
// The run parent's driver loop over bstd's run child; `run_program` is called from program.rs `run` next.
#[allow(dead_code, reason = "program.rs `run` calls run_program in the next step (wiring run-parent-io-1, 'Not wired')")]
mod run_loop;
// What a run that died on a fault or a trap says by name: the function's path, the trap's reason.
mod run_fault;
mod run_ids;
// `blaze run`'s live status line on stderr: blaze-screen's `RunStatus` driven by the run's events.
mod run_line;
mod run_names;
// `blaze run`'s program: `-p`, `--bin` and the workspace's defaults.
mod run_package;
mod roster;
mod seal;
// `blaze run`'s start list (run.md stage 7): one mutable root a (program, request shape).
mod start_list;
/// A head as a drive task (D114): its name, source and out.
mod signature;
mod single_file;
mod store;
mod sweep;
// The sweep's change detection from the system's journal of changes (plan row S1).
mod journal_sweep;
// The journal's checkpoint: a certificate's places, so a no-op replays only what came since (D150).
mod journal_checkpoint;
// The checkout's journal helper: a command's ask (journal_ask.rs) and the helper (journal_helper.rs).
mod journal_ask;
mod journal_helper;
// Wave 7 resident per-checkout daemon: synchronous kernel vnode watcher, server, and pre-runtime client.
mod daemon_client;
mod daemon_server;
mod daemon_watch;
mod graph;
mod graph_payload;
mod test_args;
mod test_cache;
mod test_drive;
mod test_loop;
mod test_run;
mod toolchain;
mod units;
mod unused_deps;
mod work;
mod check_command;
mod ci_command;
mod inspect_command;
mod engine_answer;
mod engine_answer_rows;
mod engine_check;
mod engine_dry;
mod engine_export;
mod engine_ready_edit;
mod engine_ready_check;
mod engine_bodies;
mod engine_body_facts;
mod engine_body_plan;
mod engine_plan_cfg;
mod engine_heads;
mod engine_heads_once;
mod engine_head_consts;
mod engine_head_notes;
mod engine_lines;
mod engine_lints;
mod engine_memory;
mod engine_status;
mod engine_why;
mod engine_names;
mod engine_times;
mod engine_pool;
mod engine_phase_mem;
mod engine_records;
mod engine_body_record;
mod engine_listing_record;
mod engine_member_rows;
mod engine_members_record;
mod engine_native;
mod engine_run_answer;
mod engine_scope_reads;
mod engine_start;
mod engine_scripts;
mod engine_script_reads;
mod engine_seeds;
mod engine_shelf_pages;
mod engine_store;
mod engine_tasks;
mod engine_units;

use core::fmt;

use blaze_drive::{Command, WireError};
use blaze_own::Vec;
use blaze_report::frame::Choice;
use blaze_report::frame::plain::View;
use bstd::args::Args;
use bstd::entry::Status;
use bstd::fs::{Dir, FsError, ReadWrite};
use bstd::io::Missing;
use bstd::mem::Frame;
use bstd::out::{Out, OutError};
use bstd::path::{Path, PathError};
use bstd::{Cx, Io, entry::Error};

use crate::phases::Phases;

/// Why a command stopped: each names its thing (rule 19).
#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    /// A word that is no command of blaze and never will be: the commands that do its work.
    NoSuchCommand { command: &'static str, instead: &'static str },
    /// A flag of `check` that the engine's check (check_command.rs) does not honour yet: refused
    /// by name, never quietly ignored and never run on the old checker instead (rule 3).
    NotOnEngine { flag: &'static str, why: &'static str },
    Missing(Missing),
    Path(PathError),
    Fs(FsError),
    Out(OutError),
    /// Files whose bytes, read by their tasks, were not the sweep's (`Why::Changed`): the answer
    /// is refused naming them, nothing of theirs was put; checkout-relative, one a line.
    Changed { paths: std::string::String },
    /// A member proc-macro crate's files could not be read whole for its module key
    /// (macro_files.rs), named by path.
    MacroFiles(macro_files::MacroFilesError),
    /// The checkout's leaf table is not one.
    Leaves(WireError),
    /// A file the outline cannot read: not UTF-8, or past 4 GiB.
    Text { what: &'static str },
    /// The program did not run: the stage and the function it stopped at (program.rs's form;
    /// the request names the command in [`CliError::Ran`]).
    /// Its one form: the program's manifest refused ([`ManifestRefused`]); every constructor is
    /// one (program.rs, graph.rs, run_package.rs).
    Program(ManifestRefused),
    /// The program of the command typed did not run.
    Ran { command: &'static str, error: ManifestRefused },
    /// The run parent could not start or serve the program in its own process (own_process.rs):
    /// blaze's own failure, so [`program::NOT_RUN`] (L6, D103). Boxed in the process heap
    /// ([`CliError`]'s size).
    OwnProcess(blaze_own::Box<own_process::OwnError, blaze_own::Alloc<'static>>),
    /// `blaze run` with no one program to run (run_package.rs: no such package or binary, a
    /// library, or more than one and no `-p` or `--bin`): a failure before any program starts,
    /// so [`program::NOT_RUN`] like every other one of `run` (L6).
    RunPackage(run_package::PickError),
    /// A `--grant` that cannot reach a hosted program (host_grants.rs): before it starts, so
    /// [`program::NOT_RUN`] (L6).
    HostGrant(host_grants::HostGrantError),
    /// A named origin's read path stopped the command (store.rs; a miss is not this).
    Remote(store::RemoteError),
    /// The one store under the home refused (store.rs `StoreFailure`: no `HOME`, the open, a
    /// put, the commit).
    Store(store::StoreFailure),
    /// The running binary carries no linker id, the tool identity every key starts from.
    BuildId(bstd::host::NoBuildId),
    /// The pool could not run or join a batch's tasks (its frame not made, or a wait that
    /// stopped); a task's own panic or refusal is a failed task, never this.
    Pool(bstd::task::TaskError),
    /// `blaze engine-check` stopped (engine_check.rs).
    EngineCheck(engine_check::EngineCheckError),
    /// This command's proc-macro answers could not be written (proc_runner.rs `ProcRunner::keep`).
    ProcAnswers(proc_runner::KeepError),
    /// `blaze clean` refused (clean.rs): the path that is not blaze's, named.
    Clean(clean::CleanError),
    /// `blaze gc` or store budget configuration refused (gc.rs).
    Gc(gc::GcError),
    JournalHelper(journal_helper::HelperError),
    /// A settled input a task needed could not be read (work.rs, `payload`).
    Input { name: blaze_drive::Name, why: work::InputWhy },
    /// The toolchain's shelf, which gives the target's cfg to the dependency graph (graph.rs),
    /// could not be reached.
    Shelf(toolchain::ShelfError),
    /// The pinned toolchain's manifest could not be read from the shelf.
    Toolchain(blaze_toolchain::ToolchainError),
    /// The command's own code panicked outside any task (a task's panic fails that task): its
    /// message is the panic hook's, above this on the error stream; the profile is written.
    Panicked { command: &'static str },
    /// One file's findings could not be placed or drawn (print.rs `Scratch`): the file and the
    /// frames are named.
    Draw(print::DrawError),
    /// The root manifest's lints could not be read (lints.rs `lints_of`): the key, its line and
    /// an unknown rule's word are named.
    Lints(lints::StepError),
    /// `blaze run`'s names rows (run_names.rs): the program's crates, or their record at the
    /// certificate, could not be had.
    RunNames(run_names::RunNamesError),
    /// `blaze test`'s suite stopped in blaze, not in a test (test_drive.rs): its setup, a
    /// test's start, the loop or the output. Boxed in the process heap ([`CliError`]'s size).
    Test(blaze_own::Box<test_drive::DriveError, blaze_own::Alloc<'static>>),
    /// `blaze test`'s words after `--` were refused (test_args.rs): the refusal, which borrows
    /// the words, was printed on the error stream the line above (main.rs's `StartTests` arm).
    TestArgs,
    /// `blaze test`'s words asked for a pool its plan cannot be (blaze-run zygote.rs).
    TestPool(blaze_run::zygote::PoolError),
    /// `blaze run`'s live status line failed (run_line.rs): its thread, or a write of it.
    RunLine(run_line::RunLineError),
    /// `blaze peer` failed (peer.rs).
    Peer(peer::PeerError),
    /// `run` or `test` could not start from the engine's answer (engine_start.rs).
    Start(engine_start::StartError),
    /// `blaze inspect` failed (inspect_command.rs). Boxed in the process heap (`CliError`'s size).
    Inspect(blaze_own::Box<blaze_ladder::front2::inspect::InspectError, blaze_own::Alloc<'static>>),
    /// `blaze export` failed to build or write the standalone executable (engine_export.rs).
    /// Boxed in the process heap (`CliError`'s size cap).
    Export(blaze_own::Box<engine_export::ExportCliError, blaze_own::Alloc<'static>>),
}

// THE ERROR IS SMALL: a debug build reserves a `Result<_, CliError>` and its `?` residual for
// every `?` of a function for its whole call, so each `?` on the check path cost ~2.4 KiB when
// `CliError` was 1200 bytes (wave-1119 t59: `Test` 1200, `OwnProcess` 1088, `Ran` 1040,
// `Program` 1024). The large payloads are boxed in the process heap (`&'static Heap`, never
// reset: the error outlives every request frame to `main`'s print), or kept in their own small
// form ([`ManifestRefused`]). Next to shrink: `Input` (441) and `Store` (408).
const _: () = assert!(core::mem::size_of::<CliError>() <= 512);
const _: () = assert!(core::mem::size_of::<Result<engine_check::Checked, CliError>>() <= core::mem::size_of::<engine_check::Checked>() + 512);

/// A program's manifest refused before it runs: blaze-ladder's `RunError::Manifest` words, the
/// one `RunError` `run` and `test` make before a program starts (program.rs `dev_overflow`,
/// `dev_profile_panic`; graph.rs `target`; run_package.rs `with_layout`), in its own form so
/// [`CliError`] does not carry the 1000-byte `RunError`. The words are owned as `RunError`'s
/// were, by the same box.
#[derive(Debug)]
pub struct ManifestRefused {
    pub why: Box<str>,
}

impl ManifestRefused {
    /// The refusal of `why`, kept past the frame it was said in.
    pub fn of(why: &dyn fmt::Display, frame: &bstd::mem::Frame) -> ManifestRefused {
        ManifestRefused { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) }
    }
}

impl fmt::Display for ManifestRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `RunError::Manifest`'s words (blaze-ladder run.rs).
        write!(f, "the package's manifest: {}", self.why)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(what) => write!(
                f,
                "usage: blaze check [dir] [--color ...] [--view ...] [--each] [--workers N] [--status-every S], blaze ci [dir] [--format ...] [--todo ...], blaze inspect <file.rs[::fn]> [--json] [--all-exprs], blaze run [dir] [--open] [--stats] [--workers N], blaze test [dir] [--stats] [--workers N], blaze export [dir] [-p pkg] [--bin name] [-o path] [--workers N] [--stats], blaze clean|gc|explain: {what}"
            ),
            CliError::NotOnEngine { flag, why } => write!(f, "`blaze check {flag}`: the engine's check does not take {flag} yet ({why}; check_command.rs)"),
            CliError::NoSuchCommand { command, instead } => write!(f, "blaze has no `{command}` command: {instead}"),
            CliError::Missing(missing) => write!(f, "{missing}"),
            CliError::Path(error) => write!(f, "{error}"),
            CliError::Fs(error) => write!(f, "{error}"),
            CliError::Out(error) => write!(f, "{error}"),
            CliError::Changed { paths } => write!(f, "changed while the command ran; run again (nothing of these was kept):\n{paths}"),
            CliError::MacroFiles(error) => write!(f, "{error}"),
            CliError::Leaves(error) => write!(f, "the checkout's leaf table (in a pointer of .blaze/v2/at), of this build's form, is malformed: {error}"),
            CliError::Text { what } => write!(f, "{what}"),
            CliError::Program(error) => write!(f, "the program: {error}"),
            CliError::Ran { command, error } => write!(f, "blaze {command}: {error}"),
            CliError::OwnProcess(error) => write!(f, "{error}"),
            CliError::RunPackage(why) => write!(f, "{why}"),
            CliError::HostGrant(error) => write!(f, "{error}"),
            CliError::Remote(error) => write!(f, "{error}"),
            CliError::Store(failure) => write!(f, "{failure}"),
            CliError::BuildId(error) => write!(f, "{error}: blaze keys every record by its own code and will not run without that id"),
            CliError::Pool(error) => write!(f, "{error}"),
            CliError::EngineCheck(error) => write!(f, "check: {error}"),
            CliError::ProcAnswers(error) => write!(f, "{error}"),
            CliError::Clean(error) => write!(f, "{error}"),
            CliError::Gc(error) => write!(f, "{error}"),
            CliError::JournalHelper(error) => write!(f, "{error}"),
            CliError::Input { name, why } => write!(f, "the input {name} a task needed could not be read: {why}"),
            CliError::Shelf(error) => write!(f, "{error}"),
            CliError::Toolchain(error) => write!(f, "the pinned toolchain: {error}"),
            CliError::Panicked { command } => write!(f, "blaze {command} panicked (its message is above); what it had done is in .blaze/v2/profile.txt"),
            CliError::Draw(error) => write!(f, "{error}"),
            CliError::Lints(error) => write!(f, "{error}"),
            CliError::RunNames(error) => write!(f, "{error}"),
            CliError::Test(error) => write!(f, "{error}"),
            CliError::TestArgs => write!(f, "`blaze test`'s words after `--` were refused (the line above, test_args.rs)"),
            CliError::TestPool(error) => write!(f, "`blaze test`'s pool: {error}"),
            CliError::Start(error) => write!(f, "{error}"),
            CliError::Inspect(error) => write!(f, "{error}"),
            CliError::Export(error) => write!(f, "{error}"),
            CliError::RunLine(error) => write!(f, "{error}"),
            CliError::Peer(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for CliError {}

macro_rules! from {
    ($($variant:ident($ty:ty)),+) => {
        $(impl From<$ty> for CliError {
            fn from(error: $ty) -> CliError {
                CliError::$variant(error)
            }
        })+
    };
}

from!(Missing(Missing), Path(PathError), Fs(FsError), Out(OutError), BuildId(bstd::host::NoBuildId), Lints(lints::StepError), JournalHelper(journal_helper::HelperError));

/// `blaze run` and `test` fail on any finding, lints included (bottom.md 11); `check` reports.
/// A body whose types were not checked (`body_types::TYPES_NOT_CHECKED`) says what the command
/// left out, not what is wrong with the program: it is no error, and the closing line says
/// "incomplete" for it (`check_coverage`).
fn every_rule(rule: u16) -> bool {
    // A names warning (D111's include outside its package; a test marked twice) is never an
    // error.
    !names::warning(rule)
        && rule != unused_deps::UNUSED_DEPENDENCY
        && rule != body_types::TYPES_NOT_CHECKED
        && rule != body_types::BORROWS_NOT_CHECKED
        // A task that failed only because one it needs failed: its root is the failure.
        && rule != work::WAITED
        && (body_types::BORROWS_ARE_ERRORS || rule != body_types::BORROWS)
        // A lint finding's level is the step's, decided when printed (rule 81): every step's is
        // advice today, never an error, so no lint changes the exit status (lints.rs `level_of`).
        && !lints::is_lint(rule)
}

/// What follows the command word.
struct Flags<'a> {
    dir: Option<&'a str>,
    color: Choice,
    stats: bool,
    /// `--each`: every consequence finding (a task waiting on a failed one, an item not
    /// checked) drawn one by one as well as counted under its cause (print.rs `Shown::each`).
    each: bool,
    origin: Option<&'a str>,
    origin_key: Option<&'a str>,
    /// `--grant name=value`, each a capability the program is given (run.md).
    grants: std::vec::Vec<(&'a str, &'a str)>,
    /// The words after `--`: the program's own arguments.
    program: std::vec::Vec<&'a str>,
    /// `--workers N`: at most N of a batch's tasks at once (default: one per core). For
    /// measuring how the work scales; it never changes a record.
    workers: Option<core::num::NonZeroU32>,
    /// `--status-every <seconds>`: `engine-check` writes one line of where it stands to stderr
    /// each interval (engine_status.rs `line`).
    status_every: Option<core::num::NonZeroU32>,
    /// `--view person|agent|json` (D70); none asked: by whether the output is a terminal.
    view: Option<View>,
    /// `--fix <selector>...`: the arguments that are its selectors, `start..end` of the list.
    fix: Option<(usize, usize)>,
    /// `-p <package>` and `--bin <name>`: which program `blaze run` or `blaze export` picks (run_package.rs).
    select: run_package::Select<'a>,
    /// `--open`: opt-in to opening the Explore web page in the browser on `blaze run` (default: off).
    open: blaze_run::viewer::OpenExplore,
    /// `-o <path>` / `--output <path>`: where `blaze export` writes the standalone executable.
    output: Option<&'a str>,
    /// `--names-oracle`: `check` decides every crate's names twice and compares them (D158 step
    /// 1, engine_names.rs `NamesOracle`).
    names_oracle: engine_names::NamesOracle,
    /// `--driver=auto|ready|phases`: which driver `check` runs an edit on (D187; engine_ready_edit.rs
    /// `Driver`). `auto` (the default) uses `ready` for eligible edits and falls back to `phases`.
    driver: engine_ready_edit::Driver,
    /// `--block-scopes=bodies|modules` (D189 step 4, rule 78): whether a body's blocks are
    /// modules of the crate solve (`modules`, the default) or each body's own `BodyScope`
    /// (`bodies`). Step 4: `bodies` runs the whole-crate differential inside `--names-oracle`
    /// (engine_names.rs `NamesOracle::Blocks`); the engine's records stay `modules`.
    block_scopes: blaze_scope::surface::BlockScopes,
}

impl<'a> Flags<'a> {
    pub(crate) fn empty() -> Flags<'a> {
        Flags {
            view: None,
            fix: None,
            dir: None,
            color: Choice::Auto,
            stats: false,
            each: false,
            origin: None,
            origin_key: None,
            grants: std::vec::Vec::new(),
            program: std::vec::Vec::new(),
            workers: None,
            status_every: None,
            select: run_package::Select::NONE,
            open: blaze_run::viewer::OpenExplore::No,
            output: None,
            names_oracle: engine_names::NamesOracle::Off,
            driver: engine_ready_edit::Driver::Auto,
            block_scopes: blaze_scope::surface::BlockScopes::Modules,
        }
    }
}

fn flags(args: &Args) -> Result<Flags<'_>, CliError> {
    parse_flags(args, |_| {})
}

fn parse_flags<'a>(args: &'a Args, mut package: impl FnMut(&'a str)) -> Result<Flags<'a>, CliError> {
    let mut flags = Flags::empty();
    let mut at = 2;
    while let Some(arg) = args.get(at) {
        at += 1;
        if arg == "--" {
            while let Some(word) = args.get(at) {
                at += 1;
                flags.program.push(word);
            }
            break;
        }
        let mut value = || {
            at += 1;
            args.get(at - 1).ok_or(CliError::Usage("--color, --origin, --origin-key, --grant, --workers, -p, --bin and -o each take a value"))
        };
        let selected = match arg {
            "-p" | "--package" => Some(value()?),
            _ => arg.strip_prefix("--package="),
        };
        if let Some(name) = selected {
            package(name);
            flags.select.package = Some(name);
            continue;
        }
        match arg {
            "--view" => flags.view = Some(View::parse(value()?).ok_or(CliError::Usage("--view is person, agent or json"))?),
            _ if arg.starts_with("--view=") => flags.view = Some(View::parse(&arg["--view=".len()..]).ok_or(CliError::Usage("--view is person, agent or json"))?),
            // Every word after `--fix` up to the next flag is a selector: an id or a prefix of
            // one, a code, a path, or `all` (fix.rs). Bare `--fix` with no selector fixes `all`.
            "--fix" => {
                let start = at;
                while args.get(at).is_some_and(|word| !word.starts_with("--")) {
                    at += 1;
                }
                flags.fix = Some((start, at));
            }
            "--grant" => {
                let grant = value()?;
                let pair = grant.split_once('=').ok_or(CliError::Usage("--grant takes name=value"))?;
                flags.grants.push(pair);
            }
            "--stats" => flags.stats = true,
            "--each" => flags.each = true,
            "--open" => flags.open = blaze_run::viewer::OpenExplore::Open,
            "--names-oracle" => flags.names_oracle = engine_names::NamesOracle::On,
            _ if let Some(word) = arg.strip_prefix("--driver=") => flags.driver = engine_ready_edit::Driver::parse(word).ok_or(CliError::Usage("--driver is auto, ready or phases"))?,
            _ if arg.starts_with("--block-scopes=") => {
                flags.block_scopes = blaze_scope::surface::BlockScopes::parse(&arg["--block-scopes=".len()..]).ok_or(CliError::Usage("--block-scopes is bodies or modules"))?
            }
            "--bin" => flags.select.bin = Some(value()?),
            _ if arg.starts_with("--bin=") => flags.select.bin = Some(&arg["--bin=".len()..]),
            "-o" | "--output" => flags.output = Some(value()?),
            _ if let Some(path) = arg.strip_prefix("--output=") => flags.output = Some(path),
            "--workers" => {
                let n = value()?.parse::<core::num::NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a count of at least 1"))?;
                flags.workers = Some(n);
            }
            "--status-every" => {
                let seconds = value()?.parse::<core::num::NonZeroU32>().map_err(|_| CliError::Usage("--status-every takes a count of seconds of at least 1"))?;
                flags.status_every = Some(seconds);
            }
            "--color" => flags.color = choice(value()?)?,
            "--origin" => flags.origin = Some(value()?),
            "--origin-key" => flags.origin_key = Some(value()?),
            _ if arg.starts_with("--color=") => flags.color = choice(&arg["--color=".len()..])?,
            _ if arg.starts_with("--") => return Err(CliError::Usage("an unknown flag")),
            _ if flags.dir.is_some() => return Err(CliError::Usage("one checkout directory at most")),
            _ => flags.dir = Some(arg),
        }
    }
    // D189 step 4: the block-scopes differential reports inside the names oracle, so without
    // the oracle it would have no place to report and is refused (rule 3).
    if flags.block_scopes == blaze_scope::surface::BlockScopes::Bodies {
        if flags.names_oracle != engine_names::NamesOracle::On {
            return Err(CliError::Usage("--block-scopes=bodies needs --names-oracle (D189 step 4: its differential reports in the oracle)"));
        }
        flags.names_oracle = engine_names::NamesOracle::Blocks;
    }
    Ok(flags)
}

fn choice(word: &str) -> Result<Choice, CliError> {
    match word {
        "auto" => Ok(Choice::Auto),
        "always" => Ok(Choice::Always),
        "never" => Ok(Choice::Never),
        _ => Err(CliError::Usage("--color is auto, always or never")),
    }
}

/// The process's entry: the OS root taken here, once, and handed to bstd's entry, which makes
/// the native world over it and runs [`run`] (port27/bstd-capabilities.md P3, `rt::run_with`).
///
/// COUNTED DEBT (decision D104; port27/d104-b1-entries.md, piece 2): `run` still holds a whole
/// world (`Io`) inside. It is no longer asked for as a grant (D104 bars a `main` from naming
/// `Io`), but the end state is `#[bstd::main] fn main(cx: &Cx, args: Args, env: Env, out: Out,
/// err: Out, fs: Fs, net: Net, clock: Clock, host: Host, ..)` with every function below taking
/// only the capabilities it uses. Not through `#[bstd::main]` today, so the command has no
/// manifest static (nobody asks `blaze` for one), no `bstd_hosted_start` (blaze is never
/// hosted) and reads no run spec on file descriptor 3 (no parent in the tree sets
/// `BSTD_RUN_SPEC` for it).
/// Every byte of the command's global heap counted by blaze-mem (rule 50, one memory measure):
/// what `Global` holds is the `memory held` row's "global heap outside tags", and on macOS the
/// malloc zones' committed bytes are "global allocator committed" (blaze-mem counted.rs).
#[global_allocator]
static HEAP: blaze_mem::counted::Counted = blaze_mem::counted::Counted;

fn main() -> bstd::entry::Exit {
    // Wave 7: try the resident per-checkout daemon BEFORE initializing `bstd::rt` so warm
    // `blaze check` commands skip the runtime, worker pool, and driver startup/teardown tax.
    let spawn_daemon = match daemon_client::try_run() {
        daemon_client::ClientOutcome::Exit(status) => return bstd::entry::Exit(status),
        daemon_client::ClientOutcome::Fallthrough { spawn_daemon } => spawn_daemon,
    };
    // The process's heap is a grant bstd's entry hands `main` (D102, D120), never reached for
    // here or below: every allocator under the command is its handle, a frame, or an arena.
    bstd::entry::run_with_heap_at(bstd::entry::Os::process(), telemetry_level, |cx, io, heap| run(cx, io, heap, spawn_daemon))
}

/// The run's telemetry level, chosen before the runtime starts (D178): `--stats` among the
/// command's flags (the words after the command and before `--`, as `run` parses them) keeps
/// spans and distributions and prints the report on standard error at exit; otherwise only
/// the totals, printed nowhere.
fn telemetry_level(args: &bstd::args::Args) -> bstd::telemetry::Level {
    match args.each().skip(2).take_while(|arg| *arg != "--").any(|arg| arg == "--stats") {
        true => bstd::telemetry::Level::Spans,
        false => bstd::telemetry::Level::Totals,
    }
}

/// The status: 0 for a check that passed; `blaze run`'s, the status its program's `main` ended with.
fn run(cx: &Cx, io: Io, heap: bstd::mem::Heap, spawn_daemon: bool) -> Result<Status, Error> {
    let heap = heap.root();
    let clock = io.need_clock()?;
    let mut phases = Phases::new(clock.clone());
    let out = io.need_out()?;
    let err = io.need_err()?;
    let args = io.need_args()?;
    let fs = io.need_fs()?;
    let env = io.env();
    let net = io.net();
    let host = io.need_host()?;
    let mut rng = io.need_rng()?;
    // Every capability this command can use has been extracted at the entry edge; drop `Io`
    // immediately so nothing below `run` ever holds the whole world (D104).
    drop(io);
    phases.mark("output handles");
    // `run` and `test` start a program: every error of theirs is returned before it starts
    // (the in-process run returns the program's status as `Ok`), so it is blaze's own.
    let starts_a_program = matches!(args.get(1), Some("run" | "test"));
    // A failure of `check` or `explain` is printed in one place, bstd's exit path (`error: `
    // and its message), with status 1.
    // (A `Ran` holding `RunError::Trapped` was matched here: no constructor ever made one, as
    // `Ran` is made only from `Program`, whose every constructor is a manifest refusal; a trap
    // in the program's own process is the run loop's words, run_loop.rs `LoopError`.)
    match command(cx, &args, &fs, env.as_ref(), net.as_ref(), &host, clock, &mut rng, &out, &err, &mut phases, heap, spawn_daemon) {
        // D86 and the ladder's L6: any failure of `run` or `test` before the program starts
        // (manifest, check, build, start, or a stub that stopped it) exits `program::NOT_RUN`,
        // never 1, which a program's `main` returning `Err` also exits with (std's
        // `process.rs`, `Termination for Result`). The line's form is bstd's exit path's.
        Err(error) if starts_a_program || matches!(error, CliError::Ran { .. }) => {
            err.line(cx, format_args!("error: {error}")).map_err(|e| Error::from(CliError::Out(e)))?;
            Ok(Status(program::NOT_RUN))
        }
        other => other.map_err(Error::from),
    }
}

/// Where blaze's own words go (findings, notes, the closing line): standard output for `check`
/// and `test`, standard error for `run`, whose standard output is its program's alone
/// (`blaze run | consumer` sees only what the program wrote).
fn words<'o>(command: Command, out: &'o Out, err: &'o Out) -> &'o Out {
    match command {
        Command::Run => err,
        Command::Check | Command::Test => out,
    }
}

/// What `blaze build` is told: the two commands that do what it would.
const NO_BUILD: &str = "`blaze run` compiles what the program reaches and runs it; `blaze export` writes the program as a standalone executable";

#[expect(clippy::too_many_arguments, reason = "top-level command dispatcher: each argument one specific capability extracted at main")]
fn command(
    cx: &Cx,
    args: &Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    spawn_daemon: bool,
) -> Result<Status, CliError> {
    match args.get(1) {
        // `check` IS the execution engine (check_command.rs over engine_check.rs): one checker
        // (rule 30). `engine-check` is its old name, an alias until the scripts that type it
        // are moved.
        Some("check" | "engine-check") => check_command::run(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap, spawn_daemon),
        Some("ci") => ci_command::run(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap),
        Some("inspect") => inspect_command::run(cx, args, fs, out, err, heap),
        Some("why") => engine_check::why(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap),
        // Hidden: the edit path's forward wake over the stored answer, dry (D187; engine_dry.rs).
        Some("engine-dry") => engine_dry::run(cx, args, fs, env, host, rng, out, err, phases, heap, engine_dry::Mode::Dry),
        // Hidden: the stored answer's root and page keys, to compare two drivers' answers.
        Some("engine-keys") => engine_dry::run(cx, args, fs, env, host, rng, out, err, phases, heap, engine_dry::Mode::Keys),
        // `run` and `test` start from the engine's answer (engine_start.rs, D147): the engine's
        // check, then the program or the tests over what it stored.
        Some("run") => engine_start::command(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap, Command::Run),
        Some("test") => engine_start::command(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap, Command::Test),
        // The commands are `check`, `run`, `test` and `export`: there is no `build`.
        Some("build") => Err(CliError::NoSuchCommand { command: "build", instead: NO_BUILD }),
        Some("export") => engine_export::command(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap),
        Some("explain") => explain(cx, args.get(2), out),
        // The checkout's state, and with `--store` the home's store, removed (clean.rs).
        Some("clean") => clean::run(cx, fs, env, args, out),
        // Bounded store compaction, root-LRU budget eviction, and stale old-format cleanup (gc.rs).
        Some("gc") => gc::run(cx, args, fs, env, host, &clock, rng, out, err, heap),
        // Hidden: the checkout's journal helper, run by hand until a command starts one
        // (journal_ask.rs `HelperStart`): one stream held open, asks answered on its socket.
        Some("journal-helper") => journal_helper::run(cx, fs, &clock, host, rng, env, args, out),
        // Hidden: resident per-checkout daemon (Wave 7, daemon_server.rs).
        Some("daemon") => daemon_server::run(cx, args, fs, env, net, host, clock, rng, out, err, phases, heap),
        Some("--version" | "-V" | "version") => {
            let ver = option_env!("BLAZE_RELEASE").unwrap_or(env!("CARGO_PKG_VERSION"));
            let tc = include_str!("../../../pins/toolchain").trim();
            out.line(cx, format_args!("blaze {ver} ({tc})"))?;
            Ok(Status::OK)
        }
        Some("toolchain") => {
            let tc = include_str!("../../../pins/toolchain").trim();
            out.line(cx, format_args!("Rust {tc}"))?;
            Ok(Status::OK)
        }
        Some("warm-toolchain") => engine_check::warm_toolchain(cx, args, fs, env, net, host, &clock, rng, out, err, phases, heap),
        Some("store") => store::publish_command::run(cx, args, fs, env, &clock, host, rng, out, err, heap),
        Some("peer") => peer::run(cx, args, fs, env, net, &clock, rng, out),
        Some(_) | None => Err(CliError::Usage("the command is check, ci, inspect, run, test, export, peer, clean, gc, warm-toolchain or explain")),
    }
}

/// What `blaze check --fix` ended with: `R` is the check's answer (engine_check.rs `Checked`).
enum Fixed<'f, R> {
    /// A refusal was printed; nothing was written.
    Refused,
    /// The fixes were written and the check ran again: its answer, and what it printed.
    Checked(R, print::Shown<'f>),
}

/// `blaze check --fix <selector>...` (D70; fix.rs): the check run once with nothing printed, its
/// findings chosen by `selectors` and their fixes written through `W` as one transaction (every
/// refusal decided before a byte is written), then the check run again in the same command,
/// printing what is left in the command's view. `run` is the engine's check as engine_check.rs
/// `serve` makes it (and main.rs's tests `check_in`).
fn fix_then_check<'f, W: fix::Writer + Default, R>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    selectors: &[&str],
    (caps, view, shown_root): (blaze_report::frame::Caps, View, &'f str),
    frame: &'f Frame,
    out: &Out,
    run: &mut dyn FnMut(print::Shown<'f>) -> Result<(R, print::Shown<'f>), CliError>,
) -> Result<Fixed<'f, R>, CliError> {
    let mut first = print::Shown::new(caps, view, shown_root, frame);
    first.quiet = true;
    let (_, first) = run(first)?;
    let shown = print::Shown::new(caps, view, shown_root, frame);
    match fix::apply(cx, root, &W::default(), &first.listed, selectors, frame)? {
        Err(refusal) => {
            let words = bstd::mem::format_in!(frame, "{refusal}");
            print::refused(cx, words.as_str(), &shown, frame, out)?;
            Ok(Fixed::Refused)
        }
        Ok(applied) => {
            let n = applied.fixed.len();
            let mut words = bstd::mem::format_in!(frame, "applied {n} {} in {} {}:", if n == 1 { "fix" } else { "fixes" }, applied.files, if applied.files == 1 { "file" } else { "files" });
            for id in applied.fixed.iter() {
                words.push_str(" ");
                words.push_str(id.as_str());
            }
            print::note(cx, words.as_str(), &shown, frame, out)?;
            let (finished, shown) = run(shown)?;
            Ok(Fixed::Checked(finished, shown))
        }
    }
}

/// The exit status of a `check` that found nothing while a stage it is defined to run did not
/// run: not 0, which only a complete check returns (`blaze explain 3`).
pub(crate) const INCOMPLETE: Status = Status(3);

/// The exit status of a `check` that found no error in the code it checked while it could not
/// check some items (not_checked/items.rs names each, by cause): a failure, but not of the
/// user's code (`blaze explain 4`). An error found wins: status 1, both summary lines printed.
pub(crate) const UNCHECKED_ITEMS: Status = Status(4);

/// The exit statuses `blaze` returns, each with what it means: `blaze explain <status>` prints
/// one, `blaze explain` all.
const STATUSES: [(u8, &str); 5] = [
    (0, "check: every stage `check` is defined to run ran and found nothing (run: the program's own status); clean: each folder it printed was removed, or it printed \"nothing to remove\""),
    (1, "check: an error was found, a task failed, or blaze itself failed (its message says which); run: the program's own status; clean: a folder was not blaze's, or the disk refused (named), and nothing after it was removed"),
    (3, "check: incomplete: the stages that ran found nothing, but a stage `check` is defined to run is not built yet or did not run whole (a body's borrow check refused, say); the closing line's \"NOT checked\" names which, and a note before it why; not a pass"),
    (4, "check: blaze could not check some items (each named by its cause in the \"not checked because ..\" lines, counted in \"the check is incomplete: blaze could not check N items\"); the code it did check had no error; not a pass"),
    (125, "run and test: blaze did not start the program (the manifest, a finding, a build or the start failed; its message says which); no program returned it"),
];

fn explain(cx: &Cx, asked: Option<&str>, out: &Out) -> Result<Status, CliError> {
    let wanted = match asked {
        None => None,
        Some(word) => Some(word.parse::<u8>().map_err(|_| CliError::Usage("`blaze explain` takes an exit status, a number"))?),
    };
    let mut said = false;
    for (status, words) in STATUSES.iter().filter(|(status, _)| wanted.is_none_or(|w| w == *status)) {
        out.line(cx, format_args!("exit status {status}: {words}"))?;
        said = true;
    }
    match said {
        true => Ok(Status::OK),
        false => Err(CliError::Usage("`blaze explain`: blaze returns no such exit status")),
    }
}

/// [`work::CHECK_STAGES`] split: those this command ran, and those it did not: a stage this
/// build does not run, and the names stage when the answer holds no crate's names
/// (`names_held`): never a pass printed over a stage that did not run.
/// `types_whole`: no body said its types were not checked (`print::Shown::types_unchecked`).
/// `borrows_whole`: no body said its borrows were not checked (`print::Shown::borrows_unchecked`);
/// the borrows stage also needs every body's types checked (an ill-typed body is not borrow
/// checked), and is built only with `body_types::BORROWS_ARE_ERRORS` (work.rs `CHECK_STAGES`).
fn check_coverage(names_held: bool, types_whole: bool, borrows_whole: bool, frame: &Frame) -> (Vec<&'static str, &Frame>, Vec<&'static str, &Frame>) {
    let mut ran = Vec::new_in(frame);
    let mut not = Vec::new_in(frame);
    for (stage, built, _) in work::CHECK_STAGES {
        match built
            && (names_held || stage != work::NAMES_STAGE)
            && (types_whole || stage != work::TYPES_STAGE)
            && ((types_whole && borrows_whole) || stage != work::BORROWS_STAGE)
        {
            true => ran.push(stage),
            false => not.push(stage),
        }
    }
    (ran, not)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    const MANIFEST: &str = "[package]\nname = \"wide\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    /// Smaller than the workspace's outlines together, larger than any one but `big.rs`'s. An
    /// outline takes about 2 KiB of scratch a top-level item (blaze-file-items walks each part
    /// with vectors of its own, never reused: 1,000 one-line functions passed 2 MiB, measured).
    const SCRATCH: usize = 2 << 20;
    const MODULES: usize = 40;

    /// `n` functions, about 16 bytes of text each.
    fn functions(n: usize) -> std::string::String {
        let mut text = std::string::String::new();
        for i in 0..n {
            writeln!(text, "fn f{i:05}() {{}}").expect("a string takes text");
        }
        text
    }

    /// One `blaze check` as the tests run it: the engine's (engine_check.rs `check`, as its
    /// `serve` runs it), what it printed counted, and the engine's own counts.
    struct Checked {
        /// The exit status the command returns (`engine_check::Checked::status`): always given;
        /// an `Option` so a test compares it with `Some(n)` as the command's tests do.
        status: Option<u8>,
        /// The errors it found (`Counts.errors`): notes are not counted.
        errors: u32,
        /// The tasks that failed on their own (`Counts.failures`), apart from the errors.
        failures: u32,
        /// `--fix` refused (its line printed) and wrote nothing.
        refused: bool,
        /// Each finding's (code, id), in the order listed.
        ids: std::vec::Vec<(std::string::String, std::string::String)>,
        /// Each finding's words, in the same order: what a failing assertion quotes.
        words: std::vec::Vec<std::string::String>,
        /// The profile the request wrote (print.rs `PROFILE`): what a failing count names.
        profile: std::string::String,
        /// The engine's counts (`None` when `--fix` refused, so no second check ran).
        engine: Option<engine_check::Checked>,
    }

    impl Checked {
        /// The engine's counts of a check that ran (a `--fix` refusal has none).
        fn engine(&self) -> &engine_check::Checked {
            self.engine.as_ref().unwrap_or_else(|| panic!("the --fix was refused: no check ran again"))
        }

        /// The tasks the stage `stage` entered.
        fn entered(&self, stage: blaze_engine::Stage) -> u32 {
            self.engine().stages[stage.index()].entered
        }

        /// Nothing moved: the certificate stood and no task was entered.
        fn stood(&self) -> bool {
            self.engine().certified && self.engine().entered == 0
        }
    }

    // The test modules, one a file under src/tests/ (a new test is a new file, its line here).
    mod check_whole_tests;
    mod draw_tests;
    mod extra_roots_tests;
    mod failed_call_tests;
    mod failed_roots_tests;
    mod fix_tests;
    mod members_tests;
    mod names_records_tests;
    mod names_verified_once_tests;
    mod engine_check_tests;
    mod engine_bases_worktrees_tests;
    mod engine_ready_edit_tests;
    mod names_oracle_tests;
    mod engine_const_macro_body_tests;
    mod engine_body_memory_tests;
    mod engine_body_reads_memory_tests;
    mod engine_body_coverage_tests;
    mod engine_macro_mods_tests;
    mod engine_found_files_tests;
    mod engine_skipped_targets_tests;
    mod engine_script_reads_tests;
    mod engine_script_reads_e2e_tests;
    mod engine_script_hole_tests;
    mod host_tool_facts_tests;
    mod engine_memory_tests;
    mod engine_status_tests;
    mod engine_phase_mem_tests;
    mod engine_tail_mem_tests;
    mod engine_body_frames_tests;
    mod engine_seeds_tests;
    mod engine_answer_rows_tests;
    mod engine_answer_columns_tests;
    mod engine_module_facts_tests;
    mod engine_unnamed_calls_tests;
    mod engine_expansion_tests;
    mod engine_lazy_expansions_tests;
    mod engine_owed_listing_tests;
    mod engine_head_notes_tests;
    mod engine_refusal_causes_tests;
    mod engine_refused_reads_tests;
    mod engine_macro_scope_tests;
    mod engine_builtin_reads_tests;
    mod engine_unit_heads_tests;
    mod engine_unit_files_tests;
    mod engine_member_heads_tests;
    mod engine_body_locals_tests;
    mod engine_borrow_forms_tests;
    mod engine_expansion_heads_tests;
    mod engine_head_cells_tests;
    mod engine_scope_reads_tests;
    mod engine_scope_reads_tables_tests;
    mod engine_edit_rows_tests;
    mod engine_token_keys_tests;
    mod engine_heads_once_tests;
    mod engine_heads_cover_tests;
    mod engine_heads_failed_once_tests;
    mod engine_heads_found_tests;
    mod engine_heads_split_tests;
    mod engine_heads_stop_tests;
    mod engine_body_facts_tests;
    mod engine_run_answer_tests;
    mod world_run;
    mod world_comptime;
    mod engine_listing_record_tests;
    mod engine_owners_once_tests;
    mod engine_plan_answer_tests;
    mod engine_workers_equal_tests;
    mod engine_meter_tests;
    mod include_data_tests;
    mod include_outside_tests;
    mod lint_holes_tests;
    mod lints_tests;
    mod owners_tests;
    mod refused_targets_tests;
    mod run_records_in_place_tests;
    mod script_features_record_tests;
    mod signature_grain_tests;
    mod stale_facts_tests;

    fn write(io: &Io, path: &str, text: &str) {
        write_in(io, WS, path, text);
    }

    /// [`write`] into the checkout `at`.
    fn write_in(io: &Io, at: Checkout, path: &str, text: &str) {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new(at.dir).expect("a path")).expect("open");
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
    }

    /// Two worktrees of one repository on one machine, one home (remote-read.md, "Shared across
    /// worktrees (2026-11-16)"): the same files at two paths. Every record is keyed by text alone
    /// (rules 25 and 71), so the second worktree's cold check answers from the records the first
    /// made, entering no task, and its findings are the first's and a fresh home's; a body edit
    /// in the second enters that body alone there (no names), and the first's answer stands.
    /// Ported on 2026-11-19 from store_worktree_tests.rs, which ran the old checker's request
    /// (its planted control, the checkout's path let into the old drive's keys, went with it).
    #[test]
    fn a_second_worktree_answers_from_the_first_ones_records() {
        const ONE: Checkout = Checkout { dir: "one", shown: "/one" };
        const TWO: Checkout = Checkout { dir: "two", shown: "/two" };
        // A crate root with two modules and one unresolved import: an error the answer carries.
        let tree = |io: &Io, at: Checkout| {
            write_in(io, at, "Cargo.toml", MANIFEST);
            write_in(io, at, "src/lib.rs", "mod a;\nmod b;\npub fn f() -> u8 {\n    a::g()\n}\nuse crate::nope::Thing;\n");
            write_in(io, at, "src/a.rs", "pub fn g() -> u8 { 1 }\n");
            write_in(io, at, "src/b.rs", "pub fn h() -> u8 { 2 }\n");
        };
        let at = |io: &Io, checkout: Checkout| {
            let opts = || {
                let mut opts = engine_check_tests::opts(io, 1, false);
                opts.place.checkout = checkout.dir;
                opts
            };
            check_in::<fix::Atomic>(io, checkout, &opts, View::Person, None)
        };
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        tree(&io, ONE);
        tree(&io, TWO);
        let one = at(&io, ONE);
        assert!(one.errors > 0, "the fixture's unresolved import is found, or the comparison proves nothing:\n{}", one.profile);
        assert!(one.engine().entered > 0, "the first worktree's cold check enters its tasks:\n{}", one.profile);
        let two = at(&io, TWO);
        assert_eq!(two.engine().entered, 0, "the second worktree entered tasks the first had run:\n{}\n--- the first's:\n{}", two.profile, one.profile);
        assert_eq!((two.errors, two.status, &two.ids), (one.errors, one.status, &one.ids), "the second worktree's answer is the first's");
        let fresh_io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        tree(&fresh_io, TWO);
        let fresh = at(&fresh_io, TWO);
        assert_eq!((two.errors, two.status, &two.ids), (fresh.errors, fresh.status, &fresh.ids), "the second worktree's answer is a fresh home's");
        write_in(&io, TWO, "src/b.rs", "pub fn h() -> u8 { 3 }\n");
        let edited = at(&io, TWO);
        assert_eq!(
            (edited.entered(blaze_engine::Stage::Bodies), edited.entered(blaze_engine::Stage::Names), edited.errors),
            (2, 0, one.errors),
            "a body edit in the second worktree enters that body alone, in its 2 units' rows (D191: \"Every unit that holds a body checks it; the first holder in rank keeps the body's plain name (the base), each other holder's row is the name at its view\": the library and its test build):\n{}",
            edited.profile
        );
        let again = at(&io, ONE);
        assert!(again.stood(), "the first worktree after the second's edit stands:\n{}", again.profile);
        assert_eq!((again.errors, &again.ids), (one.errors, &one.ids), "the first worktree's answer moved");
        let repeat = at(&io, TWO);
        assert!(repeat.stood(), "the second worktree's repeat stands:\n{}", repeat.profile);
    }

    /// A crate root and [`MODULES`] modules of 100 functions each (about 200 KiB of outline
    /// work a file, 8 MiB together), and `big.rs` with `big` functions.
    fn workspace(big: usize) -> Io {
        let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
        let mut lib = std::string::String::from("mod big;\n");
        for m in 0..MODULES {
            writeln!(lib, "mod m{m};").expect("a string takes text");
            write(&io, &format!("src/m{m}.rs"), &functions(100));
        }
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", &lib);
        write(&io, "src/big.rs", &functions(big));
        io
    }

    /// The fixture shelf's cfg row for this machine's triple, as the real cut writes one
    /// (scripts/src/bin/toolchain_cut.rs `target_desc`: rustc's printed cfgs plus the target's
    /// `panic="<strategy>"`, sorted): blaze-world `panic::Cuts::read` refuses a row without the
    /// `panic` atom. The one row every blaze-cli test shelf is cut with.
    pub(crate) const SHELF_CFG: &[(&str, Option<&str>)] = &[("panic", Some("unwind")), ("target_os", Some("macos")), ("unix", None)];

    /// blaze-world's fixture toolchain, cut for this machine's triple, written on the shelf of
    /// the home `/h` (as `main` would be given `HOME=/h`, started in `/`): its id.
    fn shelf(io: &Io) -> blaze_toolchain::ToolchainId {
        install_shelf(io, blaze_world::fixture::shelf(
            graph::TRIPLE,
            blaze_world::fixture::STD,
            SHELF_CFG,
            blaze_own::testing::heap().alloc(),
        ))
    }

    fn lang_shelf(io: &Io) -> blaze_toolchain::ToolchainId {
        install_shelf(io, blaze_world::fixture::shelf_with_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, blaze_own::testing::heap().alloc()))
    }

    fn install_shelf(io: &Io, (id, files): (blaze_toolchain::ToolchainId, std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>)) -> blaze_toolchain::ToolchainId {
        for (name, bytes) in files {
            let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("a path");
            io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
        }
        id
    }

    /// `blaze check` on the checkout `ws` of the home `/h`, as a person's terminal-less run
    /// draws it: the engine's check (engine_check.rs), one worker, from the stored answer.
    fn check(io: &Io) -> Checked {
        check_view::<fix::Atomic>(io, View::Person, None)
    }

    /// [`check`] in `view`, with `--fix` when `fix` gives its selectors, the fixed files written
    /// through `W`.
    fn check_view<W: fix::Writer + Default>(io: &Io, view: View, fix: Option<&[&str]>) -> Checked {
        check_opts::<W>(io, &|| engine_check_tests::opts(io, 1, false), view, fix)
    }

    /// [`check_view`] with the request's options made by `opts` (engine_check_tests.rs `opts`,
    /// one field changed by a test): called once a check, twice under `--fix`, as
    /// engine_check.rs `serve` makes them through its `Edge::opts`.
    fn check_opts<W: fix::Writer + Default>(io: &Io, opts: &dyn Fn() -> engine_check::Opts<'static>, view: View, fix: Option<&[&str]>) -> Checked {
        check_in::<W>(io, WS, opts, view, fix)
    }

    /// A checkout of the simulated machine: its folder under `/` and the root its paths are
    /// shown under.
    #[derive(Clone, Copy)]
    struct Checkout {
        dir: &'static str,
        shown: &'static str,
    }

    /// The checkout every test but the worktree ones checks.
    const WS: Checkout = Checkout { dir: "ws", shown: "/ws" };

    /// [`check_opts`] on the checkout `at` (its `opts` naming the same checkout).
    fn check_in<W: fix::Writer + Default>(io: &Io, at: Checkout, opts: &dyn Fn() -> engine_check::Opts<'static>, view: View, fix: Option<&[&str]>) -> Checked {
        check_drawn::<W>(io, at, opts, view, fix, false)
    }

    /// [`check_view`] with `--each` (every consequence drawn one by one: print.rs `Shown::each`).
    fn check_each(io: &Io, view: View) -> Checked {
        check_drawn::<fix::Atomic>(io, WS, &|| engine_check_tests::opts(io, 1, false), view, None, true)
    }

    /// [`check_in`], with `--each` when `each`, as engine_check.rs `serve` sets it from the flag.
    fn check_drawn<W: fix::Writer + Default>(io: &Io, at: Checkout, opts: &dyn Fn() -> engine_check::Opts<'static>, view: View, fix: Option<&[&str]>, each: bool) -> Checked {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new(at.dir).expect("a path")).expect("open");
        let (out, err) = (io.need_out().expect("out"), io.need_err().expect("err"));
        // Boxed in a call of its own (engine_check_tests.rs `boxed`): by value the 143 KiB meter
        // sat in this frame under the whole check.
        let mut phases = crate::tests::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
        let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, view, at.shown, frame);
        shown.each = each;
        let fs = io.need_fs().expect("files");
        let host = io.need_host().expect("host");
        let mut rng = io.need_rng().expect("rng");
        let args = io.args();
        let net = io.net();
        let mut ask = |mut shown| engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts(), &mut shown, &mut phases, &out, &err).map(|checked| (checked, shown));
        let (checked, shown) = match fix {
            None => ask(shown).unwrap_or_else(|error| panic!("the check answers: {error:?}")),
            Some(selectors) => match fix_then_check::<W, engine_check::Checked>(&cx, &root, selectors, (blaze_report::frame::Caps::LINES, view, at.shown), frame, &out, &mut ask) {
                Ok(Fixed::Checked(checked, shown)) => (checked, shown),
                // As `serve` answers a refusal: status 1, nothing written, no second check.
                Ok(Fixed::Refused) => {
                    return Checked {
                        status: Some(Status::FAILED.0),
                        errors: 0,
                        failures: 0,
                        refused: true,
                        ids: std::vec::Vec::new(),
                        words: std::vec::Vec::new(),
                        profile: std::string::String::new(),
                        engine: None,
                    };
                }
                Err(error) => panic!("check --fix answers: {error:?}"),
            },
        };
        Checked {
            status: Some(checked.status.0),
            // The errors as the closing line counts them: a note (a stage that did not run, its
            // why) is listed but is no error.
            errors: shown.counts.errors,
            failures: shown.counts.failures,
            refused: false,
            ids: shown.listed.iter().map(|l| (l.code().to_owned(), l.id.as_str().to_owned())).collect(),
            words: shown.listed.iter().map(|l| std::string::String::from_utf8_lossy(l.words).into_owned()).collect(),
            profile: engine_check_tests::profile_in(io, at.dir),
            engine: Some(checked),
        }
    }

    /// A module file looked for and not found is an absence the crate's names read: creating
    /// it reruns the crate, and the error the missing file made is gone. Control: a names
    /// record that lists no absence keeps standing, and the error with it.
    #[test]
    fn creating_a_missing_module_file_clears_its_error() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "mod x;\npub use x::f;\n");
        let missing = check(&io);
        assert!(missing.errors > 0, "a missing module file is an error:\n{}", missing.profile);
        write(&io, "src/x.rs", "pub fn f() {}\n");
        let created = check(&io);
        assert_eq!(created.failures, 0, "{}", created.profile);
        assert_eq!(created.errors, 0, "the created file clears the error: findings {:?} {:?}\n{}", created.ids, created.words, created.profile);
        // Nor a note: a fresh home on these files checks the bodies against the unit graph (the
        // members' own lock), so the edited home must too (rule 26), never say it has no graph.
        assert!(created.words.is_empty(), "the edited home lists what a fresh one does not: {:?}\n{}", created.words, created.profile);
    }

    /// An answer with errors is certified: a task's typed failure is kept with what it read, so
    /// the repeat stands on its certificate with the same errors, and fixing the cause asks
    /// again and clears them. Here member `a`'s root is not UTF-8, so it is an error at that
    /// file, and `b` depends on it: `b`'s key holds none of `a`'s text, so only its read of `a`
    /// asks it again. CONTROL: the fixed tree checked in a fresh home says the same as the
    /// edited home (rule 26), so a failure that outlived its input is caught as a difference.
    #[test]
    fn an_answer_with_errors_is_certified_and_asked_again_when_its_input_moves() {
        let tree = |io: &Io| {
            write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
            write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
            write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
            write(io, "b/src/lib.rs", "pub fn h() -> u8 {\n    a::f()\n}\n");
        };
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        tree(&io);
        {
            let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
            root.publish(&cx, &Path::new("a/src/lib.rs").expect("a path"), Content::bytes(b"pub fn f() -> u8 {\n    1\n}\n// \xff\xfe\n"), Durable::No).expect("write");
        }
        let first = check(&io);
        assert!(first.errors > 0, "a module file that is not UTF-8 is no error:\n{}", first.profile);
        let repeat = check(&io);
        assert!(repeat.stood() && (repeat.errors, repeat.failures) == (first.errors, first.failures), "the repeat: {} errors {} failures (first {} {}):\n{}", repeat.errors, repeat.failures, first.errors, first.failures, repeat.profile);
        write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
        let fixed = check(&io);
        assert_eq!((fixed.errors, fixed.failures), (0, 0), "after the fix:\n{}", fixed.profile);
        let fresh_io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        tree(&fresh_io);
        write(&fresh_io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
        let fresh = check(&fresh_io);
        assert_eq!((fixed.status, &fixed.words), (fresh.status, &fresh.words), "the fixed home answers as a fresh one");
    }

    /// The findings lines of the last command: what it printed since `before` bytes of stdout.
    fn findings_since(io: &Io, before: usize) -> std::vec::Vec<std::string::String> {
        let out = stdout(io);
        let mut lines: std::vec::Vec<std::string::String> = out[before..].lines().filter(|line| line.contains("error[") || line.contains("warning[")).map(std::string::String::from).collect();
        lines.sort_unstable();
        lines
    }

    /// One step of [`edits_answer_as_a_fresh_home`]: an edit the tape draws, on the model `files`
    /// (path, text) and on the home's checkout.
    fn edit_step(tape: &blaze_sim::Tape, io: &Io, files: &mut std::vec::Vec<(&'static str, std::string::String)>) -> Result<(), blaze_sim::Broke> {
        let harness = |what: &str| blaze_sim::Broke::Harness(std::string::String::from(what));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|_| harness("a root context"))?;
        let root = io.need_fs().map_err(|_| harness("files"))?.open_dir(&cx, &Path::new("ws").map_err(|_| harness("a path"))?).map_err(|_| harness("open"))?;
        let put = |files: &mut std::vec::Vec<(&'static str, std::string::String)>, path: &'static str, text: std::string::String| -> Result<(), blaze_sim::Broke> {
            root.publish(&cx, &Path::new(path).map_err(|_| harness("a path"))?, Content::bytes(text.as_bytes()), Durable::No).map_err(|_| harness("a write"))?;
            files.retain(|(p, _)| *p != path);
            files.push((path, text));
            Ok(())
        };
        let remove = |files: &mut std::vec::Vec<(&'static str, std::string::String)>, path: &'static str| -> Result<(), blaze_sim::Broke> {
            if files.iter().any(|(p, _)| *p == path) {
                root.remove(&cx, &Path::new(path).map_err(|_| harness("a path"))?, Durable::No).map_err(|_| harness("a remove"))?;
                files.retain(|(p, _)| *p != path);
            }
            Ok(())
        };
        let name = if tape.coin("f or g") { "f" } else { "g" };
        let text_of = |files: &std::vec::Vec<(&'static str, std::string::String)>, path: &str| files.iter().find(|(p, _)| *p == path).map(|(_, t)| t.clone());
        // Debt: a module at both places is kept out (drive.md, RUNNER 8: the home and a fresh
        // home answer it differently, tape '0 1 0 2' before this guard).
        match tape.below(8, "an edit") {
            0 => {
                remove(files, "src/x/mod.rs")?;
                put(files, "src/x.rs", format!("pub fn {name}() {{}}\n"))?;
            }
            1 => remove(files, "src/x.rs")?,
            2 => {
                remove(files, "src/x.rs")?;
                put(files, "src/x/mod.rs", format!("pub fn {name}() {{}}\n"))?;
            }
            3 => remove(files, "src/x/mod.rs")?,
            // A rename: `x.rs` moves to `x/mod.rs`.
            4 => {
                if let Some(text) = text_of(files, "src/x.rs") {
                    remove(files, "src/x.rs")?;
                    put(files, "src/x/mod.rs", text)?;
                }
            }
            // A touch: the same text written again.
            5 => {
                let text = text_of(files, "src/lib.rs").ok_or_else(|| harness("lib.rs in the model"))?;
                put(files, "src/lib.rs", text)?;
            }
            6 => put(files, "src/lib.rs", format!("mod x;\npub use x::{name};\n"))?,
            _ => {}
        }
        Ok(())
    }

    /// After every edit (a module file created, deleted, created at `x/mod.rs` instead, renamed,
    /// touched without change, its user edited, all within the clock's same tick), `check`
    /// answers as a fresh home on the same tree does (rule 26: a restore equals a fresh build),
    /// and an unchanged repeat runs no task.
    fn edits_answer_as_a_fresh_home(tape: &blaze_sim::Tape) -> Result<(), blaze_sim::Broke> {
        let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
        let mut files: std::vec::Vec<(&'static str, std::string::String)> = std::vec::Vec::new();
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "mod x;\npub use x::f;\n");
        files.push(("Cargo.toml", std::string::String::from(MANIFEST)));
        files.push(("src/lib.rs", std::string::String::from("mod x;\npub use x::f;\n")));
        let ask_now = |io: &Io| -> Result<(Checked, std::vec::Vec<std::string::String>), blaze_sim::Broke> {
            let before = stdout(io).len();
            let checked = check(io);
            let lines = findings_since(io, before);
            Ok((checked, lines))
        };
        ask_now(&io)?;
        for step in 0..tape.range(1..6, "steps") {
            edit_step(tape, &io, &mut files)?;
            let (home, home_lines) = ask_now(&io)?;
            let fresh_io = Io::simulated(SimWorld::new(3, Limits::USUAL));
            for (path, text) in files.iter() {
                write(&fresh_io, path, text);
            }
            let (fresh, fresh_lines) = ask_now(&fresh_io)?;
            // Every finding's words too, notes included: `errors` counts no note, and a note one
            // home gives and a fresh one does not is the same history dependence (rule 26).
            if (home.errors, home.status, &home_lines, &home.words) != (fresh.errors, fresh.status, &fresh_lines, &fresh.words) {
                let tree: std::vec::Vec<&str> = files.iter().map(|(p, _)| *p).collect();
                return Err(blaze_sim::Broke::Oracle(format!(
                    "differs from a fresh home at step {step} on {tree:?}: fresh {:?} {:?} {:?} {fresh_lines:?}, home {:?} {:?} {:?} {home_lines:?}; the files {files:?}\nFRESH\n{}\nHOME\n{}",
                    fresh.errors, fresh.status, fresh.words, home.errors, home.status, home.words, fresh.profile, home.profile
                )));
            }
            let (again, _) = ask_now(&io)?;
            if !again.stood() {
                return Err(blaze_sim::Broke::Oracle(format!("an unchanged repeat entered {} tasks at step {step}:\n{}", again.engine().entered, again.profile)));
            }
        }
        Ok(())
    }

    /// On the engine's check since 2026-11-19. The two controls of the old checker planted its
    /// own bugs (blaze-drive `Plant::ForgetAbsentReaders`, `Plant::HintAbsentReads`) and went
    /// with it; the oracle's own control is [`the_fresh_home_oracle_sees_a_missing_file`].
    #[test]
    fn edits_of_module_files_answer_as_a_fresh_home() {
        blaze_sim::check(blaze_sim::Config::cases(40), edits_answer_as_a_fresh_home);
    }

    /// The control of the oracle above (rule 23): a home whose module file is gone answers
    /// otherwise than a fresh home on the tree WITH the file, by the words the oracle compares.
    #[test]
    fn the_fresh_home_oracle_sees_a_missing_file() {
        let home = Io::simulated(SimWorld::new(4, Limits::USUAL));
        write(&home, "Cargo.toml", MANIFEST);
        write(&home, "src/lib.rs", "mod x;\npub use x::f;\n");
        let missing = check(&home);
        let fresh = Io::simulated(SimWorld::new(3, Limits::USUAL));
        write(&fresh, "Cargo.toml", MANIFEST);
        write(&fresh, "src/lib.rs", "mod x;\npub use x::f;\n");
        write(&fresh, "src/x.rs", "pub fn f() {}\n");
        let whole = check(&fresh);
        assert_ne!((missing.errors, &missing.words), (whole.errors, &whole.words), "the oracle's comparison sees a missing module file");
    }

    /// A missing module file is one finding at its `mod` line, naming both places; the rest of
    /// the crate is still checked (the request answers, nothing fails).
    #[test]
    fn a_missing_module_file_is_one_finding_at_its_mod_line() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() {}\nmod x;\n");
        let missing = check(&io);
        assert_eq!(missing.failures, 0, "{}", missing.profile);
        assert_eq!((missing.errors, missing.status), (1, Some(1)), "{}", missing.profile);
        let out = stdout(&io);
        assert!(out.contains("src/x.rs") && out.contains("src/x/mod.rs") && out.contains("lib.rs:2"), "{out}");
    }

    /// A syntax error inside a body is one error at its line and column, from the body's own
    /// `BodyCheck` task; the item-level outline does not say it again.
    #[test]
    fn a_syntax_error_inside_a_body_is_one_error_at_its_line() {
        let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\npub fn g() {\n    let x = ;\n}\n");
        let broken = check(&io);
        assert_eq!(broken.failures, 0, "{}", broken.profile);
        assert_eq!((broken.errors, broken.status), (1, Some(1)), "stdout {:?}", stdout(&io));
        let out = stdout(&io);
        assert!(out.contains("lib.rs:3:12"), "{out}");
    }

    /// A `use` of nothing in the project's own crate is one error at the failing segment's
    /// line and column, exit 1; its record holds the place relative to the `use` item, so a body
    /// edit above it moves the printed line with the file; the import fixed,
    /// no error is left.
    #[test]
    fn a_bad_import_is_one_error_at_its_segment() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\nuse crate::nope::Thing;\n");
        let bad = check(&io);
        assert_eq!(bad.failures, 0, "{}", bad.profile);
        let out = stdout(&io);
        assert_eq!((bad.errors, bad.status), (1, Some(1)), "{out}\n{}", bad.profile);
        assert!(out.contains("lib.rs:2:12: error[unresolved]: no `nope` in `crate`"), "{out}");
        let seen = out.len();
        write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\nuse crate::nope::Thing;\n");
        let moved = check(&io);
        let out = stdout(&io);
        assert_eq!((moved.errors, moved.status), (1, Some(1)), "{out}\n{}", moved.profile);
        assert!(out[seen..].contains("lib.rs:4:12: error[unresolved]: no `nope` in `crate`"), "{}", &out[seen..]);
        write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\nmod nope { pub struct Thing; }\nuse crate::nope::Thing;\n");
        let fixed = check(&io);
        assert_eq!((fixed.errors, fixed.status), (0, Some(0)), "{}", &stdout(&io)[seen..]);
    }

    /// One task per body, keyed by its item's text: an edit inside one body checks that body
    /// alone, while the other stands. CONTROL: the first check entered both.
    #[test]
    fn an_edit_of_one_body_reparses_that_body_alone() {
        let io = Io::simulated(SimWorld::new(8, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\npub fn g() -> u8 { 2 }\n");
        let first = check(&io);
        assert!(first.status == Some(0) && first.entered(blaze_engine::Stage::Bodies) >= 2, "{}", first.profile);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\npub fn g() -> u8 { 3 }\n");
        let edited = check(&io);
        assert_eq!((edited.status, edited.entered(blaze_engine::Stage::Bodies)), (Some(0), 2), "one body, in its 2 units' rows (D191: \"every holder checks\": the library's and its test build's)\n{}", edited.profile);
    }

    /// A body's types are checked in its own task over the crates' records. `fn f() -> u32 {
    /// "x" }` is one `types` error (the Reference, "Function body": the block's type must be
    /// the declared return type), `g` beside it clean; the repeat stands on its certificate with
    /// the error; a signature edit of `g` in the same file re-checks `g`'s body alone (its key
    /// is its item's text; `f` reads no head of `g`, D114). CONTROL: the repeat entered no body,
    /// so the edit's one is the edit's.
    /// Prints `body types: cold 1 error, repeat entered 0, signature edit entered 1`.
    #[test]
    fn a_body_type_error_is_found_over_records_and_kept() {
        let io = Io::simulated(SimWorld::new(8, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let cold = check(&io);
        let out = stdout(&io);
        assert_eq!((cold.errors, cold.status), (1, Some(1)), "{out}\n{}", cold.profile);
        assert!(cold.ids.iter().any(|(code, _)| code == "types"), "{:?}\n{out}", cold.ids);
        let repeat = check(&io);
        assert!(repeat.errors == 1 && repeat.stood(), "{}", repeat.profile);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u64 { 1 }\n");
        let edited = check(&io);
        let bodies = edited.entered(blaze_engine::Stage::Bodies);
        assert_eq!(bodies, 2, "one body, in its 2 units' rows (D191: \"every holder checks\": the library's and its test build's)\n{}", edited.profile);
        std::println!("body types: cold {} error, repeat entered {}, signature edit entered {bodies}", cold.errors, repeat.engine().entered);
    }

    /// [`engine_check_tests::opts`] with the producers' table planted: `row`'s producer is
    /// `older`'s bytes, as a blaze built from other code for that kind would be (D105).
    fn planted(io: &Io, row: Option<(blaze_ids::producer::Row, &'static str)>) -> engine_check::Opts<'static> {
        let mut opts = engine_check_tests::opts(io, 1, false);
        let producers = match row {
            Some((row, older)) => blaze_ids::producer::Producers::for_tests(b"test").planted_for_tests(row, older.as_bytes()),
            None => blaze_ids::producer::Producers::for_tests(b"test"),
        };
        opts.tools = engine_store::tools(&home::tools(&producers, lints::Lints::NONE.rules));
        opts
    }

    /// A body's record made by an older body checker is never read after a checker fix
    /// (`Row::BodyChecker` of the producers' table, in the tool, D105): the same text, checked by
    /// the older producer and then by today's, enters every body again; today's twice enters
    /// none. Without the producer in the key the second check enters none and the older answer
    /// is served.
    #[test]
    fn a_body_record_of_an_older_checker_is_made_again() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let older = check_opts::<fix::Atomic>(&io, &|| planted(&io, Some((blaze_ids::producer::Row::BodyChecker, "an older body checker"))), View::Person, None);
        let first = older.entered(blaze_engine::Stage::Bodies);
        assert!(first >= 2, "{}", older.profile);
        let today = check_opts::<fix::Atomic>(&io, &|| planted(&io, None), View::Person, None);
        assert!(today.entered(blaze_engine::Stage::Bodies) >= 2, "an older checker's body records were read\n{}", today.profile);
        let again = check_opts::<fix::Atomic>(&io, &|| planted(&io, None), View::Person, None);
        assert_eq!(again.entered(blaze_engine::Stage::Bodies), 0, "{}", again.profile);
    }

    /// Each kind's key holds its own producer (D105, home.rs `tools`): a names solver built from
    /// other code enters the names again and leaves every body record standing (its key holds
    /// the body checker's row, which did not move, and the scopes it read came out the same).
    /// The control (rule 23): the body checker's row moved instead enters every body again.
    #[test]
    fn a_moved_names_producer_leaves_body_records_standing() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let today = check_opts::<fix::Atomic>(&io, &|| planted(&io, None), View::Person, None);
        assert!(today.entered(blaze_engine::Stage::Bodies) >= 2, "{}", today.profile);
        let by_names = check_opts::<fix::Atomic>(&io, &|| planted(&io, Some((blaze_ids::producer::Row::Names, "an older names solver"))), View::Person, None);
        let scopes = by_names.entered(blaze_engine::Stage::Names);
        assert!(scopes > 0, "a moved names solver entered no names again\n{}", by_names.profile);
        assert_eq!(by_names.entered(blaze_engine::Stage::Bodies), 0, "a moved names solver entered bodies again\n{}", by_names.profile);
        let by_checker = check_opts::<fix::Atomic>(&io, &|| planted(&io, Some((blaze_ids::producer::Row::BodyChecker, "an older body checker"))), View::Person, None);
        let bodies = by_checker.entered(blaze_engine::Stage::Bodies);
        assert!(bodies >= 2, "the control: a moved body checker\n{}", by_checker.profile);
        std::println!("producers: a moved names solver entered {scopes} names and 0 bodies; a moved checker entered {bodies} bodies");
    }

    /// THE TOOLS (engine_answer.rs): the checkout's answer is one whichever blaze wrote it, so
    /// after ONE kind's producer moved (the body checker's, `Kind::BodyCheck` alone, home.rs
    /// `tools`) the phase path over the stored answer enters exactly that kind's tasks: every
    /// body, and nothing their answers did not move (their answers are the same bytes: no names,
    /// no heads, no outline, no unit, no script, no lint). The certificate does not stand on an
    /// answer of other tools. CONTROL (rule 23): with the root keyed by the tools again
    /// (`RootPlant::PerTools`, as BEL6 named it), the same check starts from no answer and
    /// enters everything: a whole check per new blaze (tempo, before 2026-11-21).
    #[test]
    fn a_moved_body_checker_enters_its_bodies_alone() {
        use blaze_engine::Stage;
        let fixture = || {
            let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
            write(&io, "Cargo.toml", MANIFEST);
            write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
            io
        };
        let older = |io: &Io| planted(io, Some((blaze_ids::producer::Row::BodyChecker, "an older body checker")));
        let io = fixture();
        let today = check_opts::<fix::Atomic>(&io, &|| planted(&io, None), View::Person, None);
        assert!(today.entered(Stage::Bodies) >= 2 && today.entered(Stage::Names) > 0, "{}", today.profile);
        let moved = check_opts::<fix::Atomic>(&io, &|| older(&io), View::Person, None);
        assert!(moved.engine().from_last && !moved.engine().certified, "THE TOOLS: the stored answer is read under other tools, never as the certificate\n{}", moved.profile);
        let bodies = moved.entered(Stage::Bodies);
        assert!(bodies >= 2, "every body's key moved with its checker\n{}", moved.profile);
        for stage in Stage::ALL.iter().filter(|stage| **stage != Stage::Bodies) {
            assert_eq!(moved.entered(*stage), 0, "{stage:?}: only the moved kind's tasks enter (names and heads 0)\n{}", moved.profile);
        }
        assert_eq!((moved.errors, moved.status), (today.errors, today.status), "the answer is today's");
        let control_io = fixture();
        let _ = check_opts::<fix::Atomic>(&control_io, &|| planted(&control_io, None), View::Person, None);
        crate::engine_answer::ROOT_PLANT.with(|cell| cell.set(crate::engine_answer::RootPlant::PerTools));
        let control = check_opts::<fix::Atomic>(&control_io, &|| older(&control_io), View::Person, None);
        crate::engine_answer::ROOT_PLANT.with(|cell| cell.set(crate::engine_answer::RootPlant::None));
        assert!(!control.engine().from_last, "CONTROL: the root keyed by the tools starts from no answer\n{}", control.profile);
        assert!(control.entered(Stage::Names) > 0 && control.entered(Stage::Bodies) >= 2, "CONTROL: it enters everything, names too\n{}", control.profile);
        std::println!("tools: a moved body checker entered {bodies} bodies and 0 else; the root keyed by the tools entered {} names, {} bodies", control.entered(Stage::Names), control.entered(Stage::Bodies));
    }

    /// What the simulated host's standard output holds.
    fn stdout(io: &Io) -> std::string::String {
        let out = io.script(|host| host.out.clone()).expect("a simulated host");
        std::string::String::from_utf8(out).expect("UTF-8")
    }

    /// `check` of a clean project says `incomplete`, names what it did and did not check, and
    /// exits 3; an item-level syntax error exits 1 with the error; `blaze explain 3` says what
    /// "incomplete" means.
    #[test]
    fn a_clean_check_says_incomplete_and_exits_3() {
        let io = Io::simulated(SimWorld::new(6, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\n");
        let clean = check(&io);
        assert_eq!(clean.status, Some(0), "{}", clean.profile);
        let out = stdout(&io);
        assert!(!out.contains("NOT checked: borrows"), "{out}");
        assert!(!out.contains("incomplete"), "{out}");
        let io = Io::simulated(SimWorld::new(6, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\nfn fn\n");
        let broken = check(&io);
        assert!(broken.status == Some(1) && broken.errors >= 1, "status {:?}, {} errors", broken.status, broken.errors);
        assert!(stdout(&io).contains("error"), "{}", stdout(&io));
        let io = Io::simulated(SimWorld::new(6, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let said = explain(&cx, Some("3"), &io.need_out().expect("out")).expect("explain 3");
        assert_eq!(said.0, 0);
        assert!(stdout(&io).starts_with("exit status 3: check: incomplete"), "{}", stdout(&io));
    }

    // A clean `blaze run` saying nothing of its own on either stream is tests/run_output.rs
    // `a_clean_run_prints_only_its_program`: the program runs by the launcher in its own
    // process (own_process.rs), which a simulated host under `cargo test` has not beside it.
    // `run` of a library answering `NoBinary` by the package's name is run_package.rs's own
    // test (`picked(&lib, ..)`) and tests/engine_start.rs's process case.

    /// The store keeps every request's records: after twelve edits, each its own answer, every
    /// record still answers (a repeat stands with no task entered, and an undo to the first
    /// tree is a fresh run's answer). CONTROL: each edit entered at least one task.
    #[test]
    fn records_outlive_the_packs_merge() {
        let io = workspace(10);
        let cold = check(&io);
        assert_eq!(cold.failures, 0, "{}", cold.profile);
        for edit in 0..12 {
            write(&io, "src/m3.rs", &format!("{}fn more{edit}() {{}}\n", functions(100)));
            let edited = check(&io);
            assert!(edited.failures == 0 && edited.engine().entered >= 1, "edit {edit}: {} entered\n{}", edited.engine().entered, edited.profile);
        }
        let again = check(&io);
        assert!(again.stood(), "a repeat after the edits:\n{}", again.profile);
        write(&io, "src/m3.rs", &functions(100));
        let undone = check(&io);
        assert!(undone.failures == 0 && (undone.status, &undone.words) == (cold.status, &cold.words), "the undo answers as the cold tree did:\n{}", undone.profile);
    }

    /// An undo back to a checked tree answers as the cold check did, and the next request (a
    /// manifest edit, which re-makes the units that list the crate root) finds each file a part
    /// names. The old checker's leaf table lost the undone file there and stopped with `NoFile`
    /// (tempo, 2026-11-16). CONTROL: the edit between them entered a task.
    #[test]
    fn an_undo_keeps_every_file_for_the_next_edit() {
        let io = workspace(10);
        let cold = check(&io);
        write(&io, "src/m3.rs", &format!("{}fn more() {{}}\n", functions(100)));
        let edited = check(&io);
        assert!(edited.engine().entered >= 1, "{}", edited.profile);
        write(&io, "src/m3.rs", &functions(100));
        let undone = check(&io);
        assert_eq!((undone.status, &undone.words), (cold.status, &cold.words), "the undo:\n{}", undone.profile);
        write(&io, "Cargo.toml", &format!("{MANIFEST}\n[dependencies]\nfar = \"1\"\n"));
        let after = check(&io);
        assert!(after.failures == 0 && !stdout(&io).contains("NoFile"), "{}", after.profile);
    }

    /// A body edit enters no crate's names: what name resolution reads of each entry is its
    /// name, visibility, attributes and form, never a body; nor does a parameter added to a
    /// function ("an edit to a parameter's, a field's or a return type [..] leaves a file's
    /// surface equal and its crate's names are not solved again"). A new item enters the names
    /// again. CONTROL: the new item's check entered names, so the zeroes are the edits'.
    #[test]
    fn a_body_edit_reruns_no_surface_and_a_signature_edit_one() {
        let io = workspace(10);
        let first = check(&io);
        assert_eq!(first.failures, 0, "{}", first.profile);
        write(&io, "src/m3.rs", &functions(100).replace("fn f00003() {}", "fn f00003() { let _x = 1; }"));
        let body = check(&io);
        assert_eq!(body.failures, 0, "{}", body.profile);
        assert_eq!((body.entered(blaze_engine::Stage::Names), body.entered(blaze_engine::Stage::Bodies)), (0, 2), "a body edit enters that one body, in its 2 units' rows (D191: \"every holder checks\"), and no names:\n{}", body.profile);
        write(&io, "src/m3.rs", &functions(100).replace("fn f00003() {}", "fn f00003(_x: u8) { let _x = 1; }"));
        let signature = check(&io);
        assert_eq!(signature.entered(blaze_engine::Stage::Names), 0, "a parameter edit enters no names:\n{}", signature.profile);
        let added = functions(100).replace("fn f00003() {}", "fn f00003(_x: u8) { let _x = 1; }") + "pub fn added() {}\n";
        write(&io, "src/m3.rs", &added);
        let item = check(&io);
        assert!(item.entered(blaze_engine::Stage::Names) > 0, "a new item enters its crate's names:\n{}", item.profile);
    }

    /// The project's own BINARY reruns its names after a signature edit (the stale answer of
    /// 2026-11-16: blaze-world's `held` kept only libraries and proc macros, so a binary had no
    /// `CrateScope` and an added `pub fn` was never seen). Cold: every crate's names (the
    /// fixture toolchain's and the binary's); a body edit: none; an added `pub fn`: the
    /// binary's and its test unit's, no other crate's (1 until 2026-11-16, when `check` took
    /// the test targets: blaze-unit-graph's `TestBin` is the same file as a crate of its own;
    /// owed: `test` as a variable of one crate's condition, drive.md "One check covers every
    /// configuration", which makes it 1 again). Control: `held` put back to libraries makes
    /// the third count 0.
    #[test]
    fn a_signature_edit_in_a_binary_reruns_its_names() {
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        write(&io, "Cargo.toml", "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "src/main.rs", "fn main() {}\n");
        let cold = check(&io);
        assert_eq!(cold.failures, 0, "{}", cold.profile);
        let names = cold.entered(blaze_engine::Stage::Names);
        assert!(names > 1, "cold resolves the toolchain's crates and the binary: {names}");
        write(&io, "src/main.rs", "fn main() { let _x = 1; }\n");
        let body = check(&io);
        assert_eq!(body.entered(blaze_engine::Stage::Names), 0, "a body edit resolves no crate's names\n{}", body.profile);
        write(&io, "src/main.rs", "fn main() { let _x = 1; }\npub fn extra(_x: u8) {}\n");
        let signature = check(&io);
        let again = signature.entered(blaze_engine::Stage::Names);
        assert!(again > 0 && again < names, "an added `pub fn` reruns the binary's names (and its test unit's), not every crate's: {again} of {names}\n{}", signature.profile);
    }

    /// A stage that did not run is named, never passed over: a package that depends on a
    /// registry crate and has no `Cargo.lock` has no dependency graph, so no crate's names are
    /// resolved; the closing line says "NOT checked: names, .." and the status is 4 (an item left unchecked). (Before:
    /// "NOT checked: types, borrows" alone, as if the names had been checked.) Control: the
    /// same package without the dependency names no `names` there.
    #[test]
    fn a_check_without_names_says_names_were_not_checked() {
        let io = Io::simulated(SimWorld::new(14, Limits::USUAL));
        write(&io, "Cargo.toml", "[package]\nname = \"lockless\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n");
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\n");
        let hollow = check(&io);
        let out = stdout(&io);
        assert_eq!(hollow.failures, 0, "{}", hollow.profile);
        assert!(
            out.contains("NOT checked: names:") && out.contains("NOT checked: types:") && out.contains("NOT checked: borrows:"),
            "{out}\n{}",
            hollow.profile
        );
        // `f` is an item left unchecked (its crate has no unit graph), so the status is
        // `UNCHECKED_ITEMS` (4), which `close` puts before `INCOMPLETE` (3).
        assert_eq!(hollow.status, Some(4), "{out}");
        let seen = out.len();
        write(&io, "Cargo.toml", "[package]\nname = \"lockless\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        let whole = check(&io);
        let out = stdout(&io);
        assert_eq!(whole.failures, 0, "{}", whole.profile);
        // The body's record of the first run said "no unit graph" and read the root's units:
        // the manifest edit moves them, so the body is checked again, not left standing with
        // the old note (rule 21; the read in body_types.rs `TypesJob::units`).
        assert_eq!(whole.status, Some(0), "{}\n{}", &out[seen..], whole.profile);
        assert!(!out[seen..].contains("NOT checked:"), "{}\n{}", &out[seen..], whole.profile);
    }

    /// A module whose items come from `include!("file")` (blaze-diff's `mod rules_gen {
    /// include!("rules_gen.rs"); }`): the included file is a part of the file that includes it
    /// (core's `include`: "the file is located relative to the current file"), so the crate's
    /// names read a listed file: the file is outlined and each of its bodies is a part.
    /// Without the `MacroCall` arm in work.rs's outline task only `src/lib.rs` is outlined
    /// ("FileItems .. made 1"), and on a project with `core` the request stops: "the names of
    /// package `blaze-diff` read `crates/blaze-diff/src/rules_gen.rs`, which no manifest's
    /// targets and no file's `mod` lines list" (blaze's checkout, 2026-11-16). This project
    /// has no `core`, so its names cannot expand `include!` ("no `include` in this scope"):
    /// what is shown here is the listing; the names' side is the real run's.
    #[test]
    fn an_included_file_is_a_part_of_the_file_that_includes_it() {
        let io = Io::simulated(SimWorld::new(15, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "mod rules {\n    include!(\"rules.rs\");\n}\npub fn f() -> u8 { 1 }\n");
        write(&io, "src/rules.rs", "pub struct A;\npub fn g() -> A { A }\npub fn h() -> A { A }\n");
        let whole = check(&io);
        let out = stdout(&io);
        assert_eq!(whole.failures, 0, "{out}\n{}", whole.profile);
        assert!(!out.contains("which no manifest's targets"), "the included file is listed:\n{out}");
        assert!(whole.entered(blaze_engine::Stage::Outlines) >= 2, "both files are outlined:\n{out}\n{}", whole.profile);
    }

    /// A crate root at `<dir>/mod.rs` (`[lib] path = "lib/mod.rs"`; blaze's `hir-ty-codegen`)
    /// is listed, and read by the names step, under its own path, while its module
    /// `lib/inner/mod.rs`, found through `mod inner;`, is the slot `lib/inner.rs`. Without
    /// `probed` in names.rs `slot_of` the request stops: "the names of package `rooted` read
    /// `lib.rs`, which no manifest's targets and no file's `mod` lines list".
    #[test]
    fn a_crate_root_named_mod_rs_is_read_under_its_own_path() {
        let io = Io::simulated(SimWorld::new(16, Limits::USUAL));
        write(&io, "Cargo.toml", "[package]\nname = \"rooted\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"lib/mod.rs\"\n");
        write(&io, "lib/mod.rs", "mod inner;\npub use inner::A;\npub fn f() -> u8 { 1 }\n");
        write(&io, "lib/inner/mod.rs", "pub struct A;\n");
        let whole = check(&io);
        assert_eq!(whole.failures, 0, "{}", whole.profile);
        assert_eq!(
            (whole.errors, whole.entered(blaze_engine::Stage::Outlines) >= 2, whole.entered(blaze_engine::Stage::Names) > 0),
            (0, true, true),
            "{}\n{}",
            stdout(&io),
            whole.profile
        );
    }

    /// `--open` is opt-in on `blaze run`: default is `OpenExplore::No`, `--open` sets
    /// `OpenExplore::Open`, and `blaze check --open` is refused by name.
    #[test]
    fn open_flag_is_opt_in_for_run_and_refused_on_check() {
        use blaze_run::viewer::OpenExplore;
        let args_of = |words: &[&str]| {
            let mut world = SimWorld::new(1, Limits::USUAL);
            world.args = words.iter().map(|&s| s.into()).collect();
            Io::simulated(world).need_args().expect("args")
        };
        let default_run = args_of(&["blaze", "run"]);
        assert_eq!(flags(&default_run).expect("default run flags").open, OpenExplore::No);

        let open_run = args_of(&["blaze", "run", "--open", "--", "--open"]);
        let parsed = flags(&open_run).expect("--open run flags");
        assert_eq!(parsed.open, OpenExplore::Open);
        assert_eq!(parsed.program, ["--open"]);

        let open_check = args_of(&["blaze", "check", "--open"]);
        let parsed_check = flags(&open_check).expect("check flags parse before command check");
        assert!(matches!(check_command::refuse_not_taken(&parsed_check), Err(CliError::Usage(_))));
    }
}
