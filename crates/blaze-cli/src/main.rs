//! `blaze`, on the new item layer (docs/v2/plan/design/drive.md, "The system side"): one command
//! is one blaze-drive [`Request`], and this binary performs its effects through bstd and nothing
//! else. `blaze check [dir]`, `blaze run [dir]`, `blaze test [dir]`; `[dir]` is the checkout,
//! relative to where blaze was started, in any spelling a person writes ([`checkout`]); `.` or no
//! argument is the start directory itself, bstd's `Fs::root`.
//!
//! | module | what |
//! |---|---|
//! | [`sweep`] | what the last answer read, followed: its leaf table, stats, opens and listings |
//! | [`store`] | records by key, behind the `Store` trait |
//! | [`units`] | the workspace's layout (blaze-manifest), the units records |
//! | [`work`] | the tasks: `Units`, `FileItems`; the rest answer what is not built |
//! | [`print`] | findings drawn by blaze-report, the closing line, and the profile |
//!
//! After the command: `--color=auto|always|never` (auto: frames on a terminal or under
//! `CLICOLOR_FORCE`, lines otherwise; blaze_report::frame::detect), `--stats` (the profile's
//! summary line on stderr), `--origin <folder> --origin-key <hex>` (the shared cache).
//!
//! Every finding a command reaches is gathered before one is drawn, then drawn in file order:
//! the request's, and under `blaze run` with any of them the program's refusals too (its
//! lowering, run without the program), so no stage's findings hide another's. The exit status
//! says there were findings; the closing line counts them.

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
mod home;
mod job_texts;
mod lints;
mod macro_files;
mod macros;
mod proc_callers;
mod proc_env;
#[cfg(test)]
mod proc_env_unknown_tests;
mod proc_runner;
mod names;
mod native_image;
mod script_modules;
mod scripts;
mod sdk;
mod not_checked;
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
// The run parent's driver loop over bstd's run child; `run_program` is called from program.rs `run` next.
#[allow(dead_code, reason = "program.rs `run` calls run_program in the next step (wiring run-parent-io-1, 'Not wired')")]
mod run_loop;
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
mod store;
mod sweep;
mod graph;
mod graph_payload;
mod test_args;
mod test_loop;
mod test_run;
mod toolchain;
mod units;
mod work;
mod engine_answer;
mod engine_check;
mod engine_bodies;
mod engine_heads;
mod engine_lines;
mod engine_why;
mod engine_names;
mod engine_times;
mod engine_pool;
mod engine_scripts;
mod engine_store;
mod engine_tasks;
mod engine_units;

use core::fmt;

use blaze_drive::{
    Answer, Ask, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Print, Request, Settled, Space, TableFull,
    WireError,
};
use blaze_ladder::refusal::{Refusal, SourceFile};
use blaze_ladder::run::RunError;
use blaze_own::{Alloc, Vec, VecDeque};
use blaze_report::frame::Choice;
use blaze_report::frame::plain::View;
use bstd::args::Args;
use bstd::entry::Status;
use bstd::env::VarName;
use bstd::fs::{Dir, FsError, ReadWrite};
use bstd::io::Missing;
use bstd::mem::Frame;
use bstd::out::{Out, OutError};
use bstd::path::{Path, PathError};
use bstd::{Cx, Io, entry::Error};

use crate::checkout::Checkout;
use crate::phases::Phases;
use crate::store::Store;
use crate::sweep::{FromMeta, LeafKind, Leaves, Rows};

/// Why a command stopped: each names its thing (rule 19).
#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    /// A command this binary does not do yet.
    NotBuilt { command: &'static str },
    /// A word that is no command of blaze and never will be: the commands that do its work.
    NoSuchCommand { command: &'static str, instead: &'static str },
    Missing(Missing),
    Path(PathError),
    Fs(FsError),
    Out(OutError),
    Drive(DriveError),
    /// A crate's names read a checkout file that no units record and no file's outline lists
    /// (`DriveError::NoPart`, named: the package and the file's path).
    Unlisted(names::Unlisted),
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
    /// The workers' settled table could not grow (twice the slots did not hold what it held).
    Table(TableFull),
    /// The program did not run: the stage and the function it stopped at (program.rs's form;
    /// the request names the command in [`CliError::Ran`]).
    Program(RunError),
    /// The program of the command typed did not run.
    Ran { command: &'static str, error: RunError },
    /// The run parent could not start or serve the program in its own process (own_process.rs):
    /// blaze's own failure, so [`program::NOT_RUN`] (L6, D103).
    OwnProcess(own_process::OwnError),
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
    /// `blaze clean` refused (clean.rs): the path that is not blaze's, named.
    Clean(clean::CleanError),
    /// A settled input a task needed could not be read (work.rs, `payload`).
    Input { name: blaze_drive::Name, why: work::InputWhy },
    /// [`CliError::Input`] while the job of `task` was made (work.rs): the task and its input.
    TaskInput { task: blaze_drive::Name, name: blaze_drive::Name, why: work::InputWhy },
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
    /// The request stood on its certificate and the store holds no record of its program's names
    /// rows (run_names.rs): the command drops it before the program starts and asks again in
    /// full, once (`command`'s `ask`). `again`: it was already asked in full, which walks and
    /// never stands, so this is blaze's bug.
    AnswerInFull { again: bool },
    /// `blaze run`'s live status line failed (run_line.rs): its thread, or a write of it.
    RunLine(run_line::RunLineError),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(what) => write!(f, "usage: blaze <check|run|test> [dir] [--color=auto|always|never] [--stats] [--workers N], blaze clean [dir] [--store], blaze explain [status]: {what}"),
            CliError::NotBuilt { command } => write!(f, "`blaze {command}` is not built on the new item layer yet (drive.md)"),
            CliError::NoSuchCommand { command, instead } => write!(f, "blaze has no `{command}` command: {instead}"),
            CliError::Missing(missing) => write!(f, "{missing}"),
            CliError::Path(error) => write!(f, "{error}"),
            CliError::Fs(error) => write!(f, "{error}"),
            CliError::Out(error) => write!(f, "{error}"),
            CliError::Drive(error) => write!(f, "{error}"),
            CliError::Unlisted(unlisted) => write!(f, "{unlisted}"),
            CliError::Changed { paths } => write!(f, "changed while the command ran; run again (nothing of these was kept):\n{paths}"),
            CliError::MacroFiles(error) => write!(f, "{error}"),
            CliError::Leaves(error) => write!(f, "the checkout's leaf table (in a pointer of .blaze/v2/at), of this build's form, is malformed: {error}"),
            CliError::Text { what } => write!(f, "{what}"),
            CliError::Table(full) => write!(f, "{full}"),
            CliError::Program(error) => write!(f, "the program: {error}"),
            CliError::Ran { command, error } => write!(f, "blaze {command}: {error}"),
            CliError::OwnProcess(error) => write!(f, "{error}"),
            CliError::RunPackage(why) => write!(f, "{why}"),
            CliError::HostGrant(error) => write!(f, "{error}"),
            CliError::Remote(error) => write!(f, "{error}"),
            CliError::Store(failure) => write!(f, "{failure}"),
            CliError::BuildId(error) => write!(f, "{error}: blaze keys every record by its own code and will not run without that id"),
            CliError::Pool(error) => write!(f, "{error}"),
            CliError::EngineCheck(error) => write!(f, "engine-check: {error}"),
            CliError::Clean(error) => write!(f, "{error}"),
            CliError::Input { name, why } => write!(f, "the input {name} a task needed could not be read: {why}"),
            CliError::TaskInput { task, name, why } => write!(f, "the input {name} of the task {task} could not be read while its job was made: {why}"),
            CliError::Shelf(error) => write!(f, "{error}"),
            CliError::Toolchain(error) => write!(f, "the pinned toolchain: {error}"),
            CliError::Panicked { command } => write!(f, "blaze {command} panicked (its message is above); what it had done is in .blaze/v2/profile.txt"),
            CliError::Draw(error) => write!(f, "{error}"),
            CliError::Lints(error) => write!(f, "{error}"),
            CliError::RunNames(error) => write!(f, "{error}"),
            CliError::RunLine(error) => write!(f, "{error}"),
            CliError::AnswerInFull { again: false } => {
                write!(f, "the run stood on its certificate and the store holds no record of its program's names rows (run_names.rs `stood`): it is asked again in full")
            }
            CliError::AnswerInFull { again: true } => write!(
                f,
                "asked again in full, the run stood on its certificate again with no record of its program's names rows (run_names.rs `found`): a request asked in full walks, so this is blaze's bug"
            ),
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

from!(Missing(Missing), Path(PathError), Fs(FsError), Out(OutError), Drive(DriveError), Table(TableFull), BuildId(bstd::host::NoBuildId), Lints(lints::StepError));

/// `blaze run` and `test` fail on any finding, lints included (bottom.md 11); `check` reports.
/// A body whose types were not checked (`body_types::TYPES_NOT_CHECKED`) says what the command
/// left out, not what is wrong with the program: it is no error, and the closing line says
/// "incomplete" for it (`check_coverage`).
fn every_rule(rule: u16) -> bool {
    // A names warning (D111's include outside its package; a test marked twice) is never an
    // error.
    !names::warning(rule)
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
    /// `--view person|agent|json` (D70); none asked: by whether the output is a terminal.
    view: Option<View>,
    /// `--fix <selector>...`: the arguments that are its selectors, `start..end` of the list.
    fix: Option<(usize, usize)>,
    /// `-p <package>` and `--bin <name>`: which program `blaze run` runs (run_package.rs).
    select: run_package::Select<'a>,
}

fn flags(args: &Args) -> Result<Flags<'_>, CliError> {
    let mut flags = Flags {
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
        select: run_package::Select::NONE,
    };
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
            args.get(at - 1).ok_or(CliError::Usage("--color, --origin, --origin-key, --grant, --workers, -p and --bin each take a value"))
        };
        match arg {
            "--view" => flags.view = Some(View::parse(value()?).ok_or(CliError::Usage("--view is person, agent or json"))?),
            _ if arg.starts_with("--view=") => flags.view = Some(View::parse(&arg["--view=".len()..]).ok_or(CliError::Usage("--view is person, agent or json"))?),
            // Every word after `--fix` up to the next flag is a selector: an id or a prefix of
            // one, a code, a path, or `all` (fix.rs).
            "--fix" => {
                let start = at;
                while args.get(at).is_some_and(|word| !word.starts_with("--")) {
                    at += 1;
                }
                if at == start {
                    return Err(CliError::Usage("--fix takes one or more selectors: an id or a prefix of one, a code, a path, or `all`"));
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
            "-p" | "--package" => flags.select.package = Some(value()?),
            "--bin" => flags.select.bin = Some(value()?),
            _ if arg.starts_with("--package=") => flags.select.package = Some(&arg["--package=".len()..]),
            _ if arg.starts_with("--bin=") => flags.select.bin = Some(&arg["--bin=".len()..]),
            "--workers" => {
                let n = value()?.parse::<core::num::NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a count of at least 1"))?;
                flags.workers = Some(n);
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
    // The process's heap is a grant bstd's entry hands `main` (D102, D120), never reached for
    // here or below: every allocator under the command is its handle, a frame, or an arena.
    bstd::entry::run_with_heap(bstd::entry::Os::process(), |cx, io, heap| run(cx, io, heap))
}

/// The status: 0 for a check that passed; `blaze run`'s, the status its program's `main` ended with.
fn run(cx: &Cx, io: Io, heap: bstd::mem::Heap) -> Result<Status, Error> {
    let heap = heap.root();
    let mut phases = Phases::new(io.need_clock()?);
    let out = io.need_out()?;
    let err = io.need_err()?;
    phases.mark("output handles");
    // `run` and `test` start a program: every error of theirs is returned before it starts
    // (the in-process run returns the program's status as `Ok`), so it is blaze's own.
    let starts_a_program = io.args().is_some_and(|args| matches!(args.get(1), Some("run" | "test")));
    // A failure of `check` or `explain` is printed in one place, bstd's exit path (`error: `
    // and its message), with status 1.
    match command(cx, &io, &out, &err, &mut phases, heap) {
        // A trap in the program's placed code (blaze-ladder trap.rs): the PROGRAM's failure,
        // said in its one line ("the program trapped (`brk #0xff02`, unreachable) in `f` +0x..,
        // called from ..") and ended with the program's status, never blaze's `NOT_RUN`.
        Err(CliError::Ran { error: RunError::Trapped { trap }, .. }) => {
            err.line(cx, format_args!("{trap}")).map_err(|e| Error::from(CliError::Out(e)))?;
            Ok(Status(blaze_ladder::trap::PROGRAM_TRAPPED))
        }
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
const NO_BUILD: &str = "`blaze run` compiles what the program reaches and runs it; `blaze export` (not built yet) writes the program as a file";

fn command(cx: &Cx, io: &Io, out: &Out, err: &Out, phases: &mut Phases, heap: &'static blaze_own::Heap) -> Result<Status, CliError> {
    let args = io.need_args()?;
    let (command, word) = match args.get(1) {
        Some("check") => (Command::Check, "check"),
        // `check` through the execution engine (engine_check.rs), `check`'s arguments; deleted
        // at the switch, when `check` itself is this path.
        Some("engine-check") => return engine_check::run(cx, io, out, phases, heap),
        Some("why") => return engine_check::why(cx, io, out, phases, heap),
        Some("run") => (Command::Run, "run"),
        Some("test") => (Command::Test, "test"),
        // The commands are `check`, `run`, `test` and, later, `export`: there is no `build`.
        Some("build") => return Err(CliError::NoSuchCommand { command: "build", instead: NO_BUILD }),
        Some("export") => return Err(CliError::NotBuilt { command: "export" }),
        Some("explain") => return explain(cx, args.get(2), out),
        // The checkout's state, and with `--store` the home's store, removed (clean.rs).
        Some("clean") => return clean::run(cx, io, &args, out),
        Some(_) | None => return Err(CliError::Usage("the command is check, run, test, clean or explain")),
    };
    let flags = flags(&args)?;
    if command != Command::Run && flags.select != run_package::Select::NONE {
        return Err(CliError::Usage("-p and --bin choose `blaze run`'s program"));
    }
    let out = words(command, out, err);
    phases.mark("argument list");
    let frame = cx.frame();
    // The frame's first allocation takes its first chunk from blaze-mem: its own phase, so the
    // line says what that costs apart from the arguments' parse.
    drop(Vec::<u8, &Frame>::with_capacity_in(1, frame));
    phases.first_chunk();
    let checkout = checkout::checkout(flags.dir, frame)?;
    phases.mark_in("arguments", frame);
    let fs = io.need_fs()?;
    let checkout_at: &str = match &checkout {
        Checkout::Under(dir) => work::held_str(dir.as_str(), frame),
        Checkout::Here => "",
    };
    let root = match checkout {
        Checkout::Under(dir) => fs.open_dir(cx, &dir)?,
        Checkout::Here => fs.root(),
    };
    let host = io.need_host()?;
    let workers = host.cores().get();
    // Every producer's identity, made once here and handed down (decision D105, rule 75): every
    // row the binary's linker id (bstd `Host::build_id`), since every producing function is in
    // this binary and runs its whole graph (producers.rs; rules 21, 52).
    let producers = producers::table(host.build_id()?.as_bytes());
    let origin = store::NamedOrigin::of(cx, &fs, io.net().as_ref(), flags.origin, flags.origin_key)?;
    // The terminal's abilities, from the conventional variables and the findings' stream, read
    // here at the edge: whether it is a terminal, and how wide.
    let env = io.env();
    let var = |name: &str| -> Option<&str> { env.as_ref()?.get(&VarName::new(name).ok()?).ok().flatten() };
    // Whether the findings' stream is a terminal: tested once, here, and given to both choices
    // it makes (the drawing's abilities, and the view: D70).
    let terminal = out.is_terminal();
    let caps = blaze_report::frame::detect(&var, terminal, out.size().map(|size| size.cols), flags.color);
    let view = blaze_report::frame::plain::view(flags.view, terminal);
    // A frame's header links to the file by its absolute path: the start directory's (`PWD`),
    // then the checkout's.
    let shown_root = match (var("PWD"), flags.dir) {
        (Some(pwd), Some(dir)) => bstd::mem::format_in!(frame, "{pwd}/{dir}"),
        (Some(pwd), None) => bstd::mem::format_in!(frame, "{pwd}"),
        (None, dir) => bstd::mem::format_in!(frame, "{}", dir.unwrap_or(".")),
    };
    let mut shown = print::Shown::new(caps, view, shown_root.as_str(), frame);
    shown.each = flags.each;
    phases.mark_in("open checkout", frame);
    let mut ran = None;
    let workers = flags.workers.map_or(u32::try_from(workers).unwrap_or(u32::MAX), |n| n.get());
    let launch = program::Launch { io, select: flags.select, grants: &flags.grants, args: &flags.program };
    // Cargo's home (D78), read here at the edge (rule 75): `CARGO_HOME`, else `<HOME>/.cargo`.
    // Its archive cache is the second place a dependency's bytes come from (fetch.rs).
    let cargo_home = match (var("CARGO_HOME"), var("HOME")) {
        (Some(cargo), _) => Some(bstd::mem::format_in!(frame, "{cargo}")),
        (None, Some(home)) => Some(bstd::mem::format_in!(frame, "{}/.cargo", home.trim_end_matches('/'))),
        (None, None) => None,
    };
    // The running blaze's folder, read once here at the edge (rule 75; main's ruling for N7):
    // the fact shim ships beside it, and a native build script's probes are answered by it.
    let exe_dir = match std::env::current_exe() {
        Err(error) => Err(fetch::ExeDirWhy::Unknown { kind: error.kind() }),
        Ok(exe) => match exe.parent().and_then(|dir| dir.to_str()) {
            Some(dir) => Ok(bstd::mem::format_in!(frame, "{dir}")),
            None => Err(fetch::ExeDirWhy::NotUtf8),
        },
    };
    // The macOS SDK, resolved once here at the edge (rule 75; sdk.rs): `SDKROOT`, else what
    // xcrun prints, by its deadline. A refusal is a value: only a script placed native fails on
    // it, by name (scripts.rs).
    let sdk = sdk::resolve(cx, var("SDKROOT"), &sdk::XcrunEnv { path: var("PATH"), developer_dir: var("DEVELOPER_DIR"), home: var("HOME") }, frame);
    // The one store a user (drive.md, "The store under the command"), opened here, outside the
    // request: every record lent from it outlives the request, and its ONE commit, carrying the
    // records and the roots, comes after the answer is printed (below).
    let opened = store::open_for_command(cx, io, var("HOME"), &root, checkout_at, phases.clock(), err)?;
    // The request as this command makes it; it takes what it prints into and gives it back.
    let mut ask = |mut shown, ran: &mut Option<u8>, phases: &mut Phases, watch: &dyn blaze_screen::Watch| {
        let exe_dir = exe_dir.as_ref().map(|dir| dir.as_str()).map_err(|why| *why);
        // A run that stood on its certificate with no record of its program's names rows is
        // dropped before its program starts and asked again in full, once (run_names.rs; a
        // request asked in full walks, so a second one is blaze's bug, named).
        let mut in_full = false;
        loop {
            let finished = request(cx, &root, (checkout_at, var("SSL_CERT_FILE"), var("SSL_CERT_DIR"), var("HOME"), cargo_home.as_ref().map(|cargo| cargo.as_str()), var("PWD"), blaze_toolchain::PINNED, sdk, exe_dir), origin.as_ref(), (&opened.home, &opened.lent, opened.open_ns), (command, word), workers, &producers, work::Sizes::USUAL, &launch, watch, frame, out, err, ran, &mut shown, phases, heap, in_full);
            if !in_full && matches!(finished, Err(CliError::AnswerInFull { .. })) {
                in_full = true;
                continue;
            }
            break finished.map(|finished| (finished, shown));
        }
    };
    // `blaze run`'s live status line on the error stream when it is a terminal (run_line.rs):
    // the run's events go to it; it is erased, and its thread joined, before `asked` is had.
    let line = run_line::LineOpts::new(&|name| var(name).map(std::string::String::from), err, flags.color);
    let line = run_line::LineOpts { live: line.live && command == Command::Run, ..line };
    let clock = phases.clock().clone();
    // The whole request, behind the command's one edge for a panic (rule 20): a command that
    // fails or panics still leaves its profile, so why it stopped is read, not guessed (rule 17).
    let asked = work::unwinding(|| {
        let lined = run_line::watched(cx, io, err, &clock, line, |watch| match flags.fix {
            None => ask(shown, &mut ran, phases, watch).map(Some),
            Some((start, end)) => {
                if command != Command::Check {
                    return Err(CliError::Usage("--fix is `blaze check`'s"));
                }
                let mut selectors: Vec<&str, &Frame> = Vec::with_capacity_in(end - start, frame);
                selectors.extend((start..end).filter_map(|at| args.get(at)));
                match fix_then_check::<fix::Atomic>(cx, &root, &selectors, (caps, view, shown_root.as_str()), frame, out, &mut |shown| ask(shown, &mut ran, phases, watch))? {
                    Fixed::Refused => Ok(None),
                    Fixed::Checked(finished, again) => Ok(Some((finished, again))),
                }
            }
        });
        lined.map_err(CliError::RunLine).and_then(|asked| asked)
    });
    let (mut finished, shown) = match asked {
        Ok(Ok(Some(done))) => done,
        Ok(Ok(None)) => {
            store::after_answer_printed(cx, opened.lent, flags.stats, phases.clock(), err)?;
            return Ok(Status::FAILED);
        }
        Ok(Err(error)) => {
            print::profile_failed(cx, &root, &error, phases, frame)?;
            return Err(error);
        }
        Err(work::Panicked) => {
            let error = CliError::Panicked { command: word };
            print::profile_failed(cx, &root, &error, phases, frame)?;
            return Err(error);
        }
    };
    phases.frames = Some(finished.high);
    finished.answer.profile.coordinator_ns = phases.request.steps;
    let p = &finished.answer.profile;
    if flags.stats {
        err.line(
            cx,
            format_args!(
                "blaze: {} runs, {} store reads ({} found), {} opens, certificate {}; {:.3} ms in main; .blaze/v2/profile.txt",
                p.runs(),
                p.gets,
                p.hits,
                p.opens,
                if p.certified { "stood" } else { "missed" },
                phases.in_main() as f64 / 1e6,
            ),
        )?;
    }
    let status = close(cx, command, &finished, &shown, ran, phases.in_main(), frame, out);
    phases.mark_in("answer printed", frame);
    // The answer is printed: the profile is written after it, in place, never synced.
    print::profile(cx, &root, &finished.answer, phases, frame)?;
    // Then the store's one commit: this command's records and its roots in one flip.
    store::after_answer_printed(cx, opened.lent, flags.stats, phases.clock(), err)?;
    status
}

/// A request's closing line and its exit status, the one home of both (a command test calls
/// it as `command` does). Nothing follows a program that ran: its output is the command's.
/// `check` names what it did not check, and never says `ok` over it (decision D61 item 5).
#[expect(clippy::too_many_arguments, reason = "the request's parts, each read once")]
fn close(cx: &Cx, command: Command, finished: &Finished<'_>, shown: &print::Shown<'_>, ran: Option<u8>, in_main: u64, frame: &Frame, out: &Out) -> Result<Status, CliError> {
    let (ran_stages, not_stages) = check_coverage(finished.names_held, shown.types_unchecked == 0, shown.borrows_unchecked == 0, frame);
    let coverage = (command == Command::Check && !not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    // How many items `check` could not check: also `run`'s when its program did not start, as
    // its gate refuses a start over one (`Gate::Before`); 0 for `test`, and for a program that ran.
    let mut unchecked = 0u32;
    if ran.is_none() {
        let tail = (command == Command::Test).then_some("tests are not run yet: this build of blaze checks them");
        // The items `check` could not check: their lines, one a cause, were drawn with the
        // other notes (main.rs `draw`), before the findings; here only their count.
        if matches!(command, Command::Check | Command::Run) {
            unchecked = not_checked::items::lines(&shown.unchecked, frame, &mut |_| Ok(()))?;
        }
        // Each stage the closing line says was NOT checked because this build does not build
        // it, with its reason (work.rs `CHECK_STAGES`): a body's refusal names its own, but a
        // stage not built refuses no body, and "NOT checked" must never stand alone (rule 19).
        if coverage.is_some() {
            for (stage, built, why) in work::CHECK_STAGES {
                if !built {
                    print::note(cx, bstd::mem::format_in!(frame, "NOT checked: {stage}: {why}").as_str(), shown, frame, out)?;
                }
            }
        }
        print::closing(cx, shown, finished.checked, in_main, Some(finished.answer.profile.runs()), tail, coverage, frame, out)?;
        // Its own summary line, beside the closing line's counts (never "0 errors" beside it):
        // every item not checked, those behind a failure too. A body not run because an input
        // failed is an item its cause's line above named.
        if matches!(command, Command::Check | Command::Run) {
            let not_run = shown.not_run.iter().filter(|task| task.kind == blaze_drive::Kind::BodyCheck.name()).count();
            let own = unchecked.saturating_add(u32::try_from(not_run).unwrap_or(u32::MAX));
            if let Some(words) = not_checked::items::summary(own, shown.behind, frame) {
                print::note(cx, words.as_str(), shown, frame, out)?;
            }
        }
    }
    // A task not run because an input failed is not counted as an error, but the command did not
    // do what it was asked: it fails, whatever the input's own finding counted.
    if shown.counts.errors > 0 || shown.counts.failures > 0 || finished.answer.failed > 0 || !shown.not_run.is_empty() {
        // A program that did not run because of what was found is blaze's refusal (L6):
        // `NOT_RUN`, never 1, a status the program itself could have ended with.
        return Ok(match (command, ran) {
            (Command::Run | Command::Test, None) => Status(program::NOT_RUN),
            _ => Status::FAILED,
        });
    }
    match command {
        // No error in the code it checked, but items it could not check: a failure of its own.
        Command::Check if unchecked > 0 => Ok(UNCHECKED_ITEMS),
        // Nothing found, but a stage `check` is defined to run did not: not a pass.
        Command::Check if coverage.is_some() => Ok(INCOMPLETE),
        Command::Check => Ok(Status::OK),
        // No error, and items the check could not check: the gate did not start the program
        // (`Gate::Before`). blaze's refusal, as over an error (L6): never a status the program
        // itself could have ended with.
        Command::Run if ran.is_none() && unchecked > 0 => Ok(Status(program::NOT_RUN)),
        // The program ran at the machine's `StartProgram`: its status is the command's.
        Command::Run => ran.map(Status).ok_or(CliError::Drive(DriveError::Finished)),
        Command::Test => Ok(Status::FAILED),
    }
}

/// What `blaze check --fix` ended with.
enum Fixed<'f> {
    /// A refusal was printed; nothing was written.
    Refused,
    /// The fixes were written and the check ran again: its answer, and what it printed.
    Checked(Finished<'f>, print::Shown<'f>),
}

/// `blaze check --fix <selector>...` (D70; fix.rs): the check run once with nothing printed, its
/// findings chosen by `selectors` and their fixes written through `W` as one transaction (every
/// refusal decided before a byte is written), then the check run again in the same command,
/// printing what is left in the command's view. `run` is the request as `command` makes it.
fn fix_then_check<'f, W: fix::Writer + Default>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    selectors: &[&str],
    (caps, view, shown_root): (blaze_report::frame::Caps, View, &'f str),
    frame: &'f Frame,
    out: &Out,
    run: &mut dyn FnMut(print::Shown<'f>) -> Result<(Finished<'f>, print::Shown<'f>), CliError>,
) -> Result<Fixed<'f>, CliError> {
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
    (3, "check: incomplete: the stages this build runs found nothing, but a stage `check` is defined to run is not built yet (the closing line's \"NOT checked\" names which); not a pass"),
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
/// (`Finished::names_held`): never a pass printed over a stage that did not run.
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

/// A request's end: its answer, and how many source files it checked.
struct Finished<'f> {
    answer: Answer,
    checked: u32,
    /// The most each frame held: the profile's `frames` line.
    high: work::HighWater,
    /// The tasks that failed, each with its file and words: one table on the command's heap,
    /// freed with the answer once it is printed (never in the command's frame).
    failed: work::Failures<'f>,
    /// The crates whose names this command resolved (names.rs): the closing note's count.
    resolved: u32,
    /// Whether the answer holds the crates' names: `false` when the stage did not run, which
    /// the closing line then names ("NOT checked: names, ..").
    names_held: bool,
}

/// A sealed last answer and leaf table (`seal::seal`) as the request reads them: its `Last`
/// bytes (none when of an older form) and its table. An older or torn record is a notice and
/// no answer: the request starts cold and writes its own over it.
fn open_sealed<'m>(held: &'m [u8], notices: &mut Vec<Notice, &Frame>) -> Result<(Option<&'m [u8]>, Leaves<'m>), CliError> {
    let bytes = match home::current(seal::PATH, held, seal::SEAL_MAGIC) {
        Ok(bytes) => bytes,
        Err(older) => {
            notices.push(Notice::Older(older));
            return Ok((None, Leaves::EMPTY));
        }
    };
    let (last, leaves) = match seal::open(bytes, seal::Plant::None) {
        Ok(opened) => opened,
        Err(torn) => {
            notices.push(Notice::Torn(torn));
            return Ok((None, Leaves::EMPTY));
        }
    };
    let (leaves, older) = Leaves::of(leaves)?;
    if let Some(older) = older {
        notices.push(Notice::Older(older));
    }
    let last = match home::current(LAST, last, blaze_drive::answers::LAST_MAGIC) {
        Ok(bytes) => Some(bytes),
        Err(older) => {
            notices.push(Notice::Older(older));
            None
        }
    };
    Ok((last, leaves))
}

/// The rows the answer keeps (the leaf table): each source file the request holds, each absent
/// probe, and the layout's reads.
fn kept<'f>(req: &Request<'_>, followed: &sweep::Followed<'_>, rows: &Rows<'_>, frame: &'f Frame) -> Rows<'f> {
    let mut kept = Rows::new_in(frame);
    for (from, slots_only) in [(&followed.now, true), (rows, false)] {
        for leaf in from.rows.iter() {
            let path = from.path(leaf);
            // An absent probe is kept while anything may probe it: its prober is not known here
            // (a gap: one dropped stays until the table is made again cold).
            let keep = match leaf.kind {
                LeafKind::Slot => leaf.absent() || req.holds(path.as_bytes()),
                LeafKind::Manifest | LeafKind::Listing | LeafKind::HomeListing | LeafKind::MacroFiles | LeafKind::MacroFile | LeafKind::MacroFolder => !slots_only,
            };
            if keep {
                kept.push(leaf.kind, leaf.flags, path, leaf.stamp, leaf.id);
            }
        }
    }
    kept
}

/// One request, its effects performed in the order they come: gets and puts at once, task runs
/// one after another on this thread (the pool is the next piece: `workers` is what the machine
/// emits to). `ran` is the status of the program `blaze run` started, once it ended.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle or fact of it")]
fn request<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    // The checkout, relative to where blaze was started: the fetched records' driver's root;
    // then `SSL_CERT_FILE` and `SSL_CERT_DIR`, where the HTTPS client's roots are looked for;
    // then `HOME`, cargo's home (`CARGO_HOME`, else `<HOME>/.cargo`: its archive cache, fetch.rs)
    // and `PWD`, where the toolchain's shelf is (the target's cfg, graph.rs); then the toolchain
    // the command opens (`blaze_toolchain::PINNED` from `main`, a fixture's in tests); then
    // the macOS SDK, resolved once at the edge (sdk.rs): the native build-script runner takes its
    // folder, the scripts' keys its identity (scripts.rs); then the running blaze's folder, where
    // the fact shim is (D112, N7).
    (checkout_at, cert_file, cert_dir, home, cargo_home, start, toolchain, sdk, exe_dir): (
        &'f str,
        Option<&'f str>,
        Option<&'f str>,
        Option<&'f str>,
        Option<&'f str>,
        Option<&'f str>,
        blaze_toolchain::ToolchainId,
        Result<blaze_apple_sdk::Sdk<'f>, blaze_apple_sdk::SdkRefused<'f>>,
        Result<&'f str, fetch::ExeDirWhy>,
    ),
    origin: Option<&store::NamedOrigin>,
    // The one store, opened by the command's function and committed by it after the answer is
    // printed: its home folder, the lent every record is borrowed from, and the open's time.
    (store_home, lent, open_ns): (&store::Home, &store::CommandLent, u64),
    (command, word): (Command, &'static str),
    workers: u32,
    // Every producer's id (D105), made once at the entry: the whole tool and each kind's below.
    producers: &blaze_ids::producer::Producers,
    sizes: work::Sizes,
    launch: &program::Launch<'_>,
    // The sink `blaze run`'s events go to (blaze-screen `Watch`): the live line on a terminal,
    // else `Unwatched`.
    watch: &dyn blaze_screen::Watch,
    frame: &'f Frame,
    out: &Out,
    err: &Out,
    ran: &mut Option<u8>,
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    // The process's heap, handed from `main` (D120): the machine's, the work list's and every
    // record that leaves for the store when its task ends are made on it.
    heap: &'static blaze_own::Heap,
    // Asked again in full (`CliError::AnswerInFull`): no last answer and no leaf table, so the
    // request walks from the units down and never stands on its certificate.
    in_full: bool,
) -> Result<Finished<'f>, CliError> {
    let cx = cx.span("blaze request");
    let began = phases.now();
    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
    // The whole tool (the certificate, the pointer, the walk from the last answer, the store's
    // producer name) and each kind's (its tasks' keys): home.rs.
    // `check` lints by the step and the added rules the root manifest names (lints.rs
    // `lints_of`), read once a command; every other command runs no rule.
    let lints = match command {
        Command::Check => {
            let mut manifest = bstd::mem::Bytes::new_in(frame, "the root manifest, for its lint step");
            root.read_all(&cx, &Path::new("Cargo.toml")?, &mut manifest)?;
            let text = core::str::from_utf8(manifest.as_slice()).map_err(|_| CliError::Text { what: "the root `Cargo.toml` is not UTF-8" })?;
            lints::lints_of(text)?
        }
        _ => lints::Lints::NONE,
    };
    shown.lints = lints.step;
    let (tool, tools) = (home::tool(producers), home::tools(producers, lints.rules));
    // The last answer and the leaf table (drive.md ruling 3): one record, sealed (seal.rs), named
    // by this checkout's root for this tool and request shape (`store::last_root`), read in
    // place. Another build of blaze and another command each have their own root, so neither
    // takes the answer this one walks forward from. No root, another form's or torn, it is
    // absent: a note after the findings, and the request answers in full.
    // `run`'s program is the picked binary (run_package.rs `Program::root`), named from its path
    // before the layout is read: the shape holds it, so each program has its own pointer and
    // certificate (native_image.rs `program_name`).
    // Which program is chosen first (run_package.rs: `-p`, `--bin`, the workspace's defaults),
    // so a run with no one program to run is refused before any work.
    let picked = match command {
        Command::Run => Some(run_package::pick(&cx, root, launch.select, frame)?),
        Command::Check | Command::Test => None,
    };
    let program = picked.map(|picked| native_image::program_name(picked.root));
    // No facts yet: no worker reads an environment variable through the machine (A2b item 5).
    // `run` requires its check first (run.md D124, "The gate is before the start"): `Before`
    // starts the program only over a check with no error and no item it could not check.
    // `test` holds its verdicts until the check ends beside it (stage 4); `check` ignores it.
    let gate = match command {
        Command::Run => Gate::Before,
        Command::Check | Command::Test => Gate::Beside,
    };
    let ask = Ask { command, args: b"", gate, space: Space::NONE, program, facts: &[] };
    let shape = blaze_drive::request::shape_of(&ask);
    let pointer = blaze_drive::pointers::PointerName::of(tool, &shape);
    let last_root = store::last_root(checkout_at, &pointer);
    // Each a note after the findings (`draw`).
    let mut notices: Vec<Notice, &Frame> = Vec::new_in(frame);
    let (last, leaves) = match store::last(lent, &last_root)? {
        None => (None, Leaves::EMPTY),
        Some(held) => open_sealed(held.bytes, &mut notices)?,
    };
    // A pointer that says another tool or shape than its name does (two (tool, shape)s whose
    // names' 64 bits collide; before the names, another build's or command's `.blaze/v2/last`)
    // is no answer of this request's: the machine does not walk forward from it, and the leaf
    // table beside it would keep the layout from being read here, so `Start` was handed no
    // dependency graph and no `UnitGraph`, `Fetch` or `CrateScope` ran, with "0 errors" over
    // the hole (blaze's own checkout after a new build, 2026-11-16). It is absent: the
    // request starts cold and writes its own over it.
    let (last, leaves) = match last {
        Some(bytes) if !blaze_drive::request::walks_from(bytes, tool, &shape).map_err(|error| CliError::Drive(DriveError::LastFile(error)))? => (None, Leaves::EMPTY),
        _ => (last, leaves),
    };
    let (last, leaves) = if in_full { (None, Leaves::EMPTY) } else { (last, leaves) };
    phases.mark_in("pointer read (last answer and leaf table)", frame);
    let stamps = FromMeta::now(phases.clock());
    let sweep_plant = match sizes.plant {
        work::Plant::Sweep(plant) => plant,
        _ => sweep::Plant::None,
    };
    // blaze's folder in the home holds a fact the answer reads (the proc-macro module store's
    // listing, a `HomeListing` row): the sweep follows it as a second tree.
    let home_dir = Some(store_home.dir());
    let followed = sweep::Follow { stamps, plant: sweep_plant }.follow(&cx, root, home_dir, &leaves, frame)?;
    phases.mark_in("sweep", frame);
    // No table: nothing to follow; the layout discovers forward from the root manifest, and the
    // drive opens each file as a record names it.
    let cold = leaves.len() == 0;
    // The machine and the layout allocate through a one-word handle; the frame carries no header
    // of its own, so it is erased into one here, on this stack, for this request.
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut facts = work::Facts { stamps, rows: Rows::new_in(frame), found: sweep::Found::new_in(frame), laid: None, plant: sizes.plant, workers, fetch_env: fetch::FetchEnv { clock: phases.clock().clone(), checkout: checkout_at, cert_file, cert_dir, home, cargo_home, start, toolchain, fs: launch.io.need_fs()?, sdk, exe_dir, heap }, macros: None, names: None, bodies: body_types::Kept::default(), scripts: None, producer: producers.get::<blaze_ids::producer::Whole>(), lints: lints.rules };
    if cold || followed.layout_moved {
        facts.laid = Some(units::lay(&cx, root, &stamps, &mut facts.rows, toolchain, heap, frame, alloc)?);
        // The proc-macro module store's listing, read with the layout: in the units' source, so
        // a module that comes or goes moves the certificate and the dependency graph's source,
        // and with it each `MacroModule` part's (macros.rs).
        sweep::read_home_listing(&cx, home_dir, &stamps, macros::MODULE_LISTING, &mut facts.rows, frame)?;
        // The build-script module store's listing, the same way: in every `Script` part's
        // source, so a module the bootstrap builder adds or replaces asks again (scripts.rs).
        sweep::read_home_listing(&cx, home_dir, &stamps, scripts::MODULE_LISTING, &mut facts.rows, frame)?;
    } else {
        for leaf in followed.now.rows.iter().filter(|leaf| leaf.kind != LeafKind::Slot) {
            facts.rows.push(leaf.kind, leaf.flags, followed.now.path(leaf), leaf.stamp, leaf.id);
        }
    }
    phases.mark_in("layout", frame);
    // The units' source: what the layout read. Equal reads, equal layout.
    let units = sweep::units_source(facts.rows.rows.iter().map(|leaf| (facts.rows.path(leaf), leaf)), frame);
    // A cold checkout another checkout's answer may cover (a second worktree's first command):
    // the last answer for today's units' source, whichever checkout of this machine wrote it
    // (`store::seed_root`), followed as this checkout's own table would be. Its reads hashed
    // here key the certificate, so equal text is a hit that runs and puts nothing (the drive's
    // `certified`); other text walks forward from that answer. Trusted for nothing its key does
    // not say: a seed of other text only misses. Before, such a command walked the whole build
    // from records and wrote its root, page and certificate again (store_worktree_tests.rs:
    // "written 3").
    let seed_root = store::seed_root(&units, &pointer);
    let mut seeded: Option<&[u8]> = None;
    let (followed, last, cold) = match (cold && !in_full).then(|| store::last(lent, &seed_root)).transpose()?.flatten() {
        Some(held) => match open_sealed(held.bytes, &mut notices)? {
            (Some(bytes), seed_leaves) if blaze_drive::request::walks_from(bytes, tool, &shape).map_err(|error| CliError::Drive(DriveError::LastFile(error)))? => {
                seeded = Some(bytes);
                (sweep::Follow { stamps, plant: sweep_plant }.follow(&cx, root, home_dir, &seed_leaves, frame)?, Some(bytes), false)
            }
            _ => (followed, last, cold),
        },
        None => (followed, last, cold),
    };
    phases.mark_in("seed (another checkout's last answer)", frame);
    // The dependency graph's job, when a lock was read with the layout: its source needs the
    // units' source and the target's cfg, which the toolchain's shelf states.
    if let Some(laid) = facts.laid.as_mut()
        && let (Some(lockfile), Some(parts)) = (laid.lock.as_ref(), laid.parts.take())
    {
        // THE COMMAND'S PANIC STRATEGY, second spot (the unit graph's cfg): the program's
        // profile fact, the same as run_parent.rs `builder_for`'s; each desc's `cfg(panic)` is
        // derived from it by blaze-world `panic::unit_panic`. A test build unwinds throughout
        // and does not read the profile (`panic::TEST_PROFILE`, Cargo profiles.html#panic).
        let program = match command {
            Command::Test => blaze_world::panic::TEST_PROFILE,
            _ => program::dev_profile_panic(&cx, root, frame)?,
        };
        let target = graph::target(&cx, &facts.fetch_env, program, &blaze_unit_graph::UnitKind::Lib, frame)?;
        let host = graph::target(&cx, &facts.fetch_env, program, &blaze_unit_graph::UnitKind::BuildScript, frame)?;
        laid.graph = Some(graph::Graph::new(parts, lockfile, units, target, host, matches!(command, Command::Test | Command::Check), heap, frame));
        phases.mark_in("the target's cfg", frame);
    }
    let mut files: Vec<FileEntry<'_>, &Frame> = Vec::with_capacity_in(followed.now.rows.len(), frame);
    let mut gone: Vec<&[u8], &Frame> = Vec::new_in(frame);
    for (i, leaf) in followed.now.rows.iter().enumerate() {
        let path = followed.now.path(leaf);
        if !matches!(leaf.kind, LeafKind::Listing | LeafKind::HomeListing | LeafKind::MacroFiles | LeafKind::MacroFile | LeafKind::MacroFolder) && leaf.absent() && followed.was[i].0 & sweep::ABSENT == 0 {
            gone.push(path.as_bytes());
        }
        if leaf.kind != LeafKind::Slot || leaf.absent() {
            continue;
        }
        facts.found.note(path, leaf.flags);
        files.push(FileEntry { path: path.as_bytes(), id: leaf.id, changed: followed.changed(i), manifest: false });
    }
    // The manifests the layout read: held while it is, and the files their findings print in.
    for leaf in facts.rows.of(LeafKind::Manifest).filter(|leaf| !leaf.absent()) {
        let path = facts.rows.path(leaf);
        let was = followed.now.rows.iter().enumerate().find(|(_, old)| old.kind == LeafKind::Manifest && followed.now.path(old) == path);
        let changed = was.is_none_or(|(i, _)| followed.was[i] != (0, leaf.id));
        let path: &[u8] = Vec::from_slice_in(path.as_bytes(), frame).leak();
        files.push(FileEntry { path, id: leaf.id, changed, manifest: true });
    }
    files.sort_unstable_by(|a, b| a.path.cmp(b.path));
    files.dedup_by(|a, b| a.path == b.path);
    gone.sort_unstable();
    gone.dedup();
    let config = Config { tool, tools, workers, most_entries: 64, most_jobs: 1 << 24, is_error: every_rule };
    // The machine allocates from the process's heap, handed from `main` (rule 11): what it hands out to be
    // performed (a record's bytes to `Put`, a page) is freed once performed, so the request's
    // footprint is its keys, rows and findings, not every record it wrote (rule 51: a record
    // leaves for the store when its task ends). A frame would hold each until the command ended.
    let drive_plant = match sizes.plant {
        work::Plant::Drive(plant) => plant,
        _ => blaze_drive::Plant::None,
    };
    // A body whose types were not checked is the item `check` counts for its status 4
    // (not_checked/items.rs): the same finding keeps `run`'s program from starting.
    // A crate's names are read by cell (names/sections.rs `cell_of`): a body that read cells of
    // a `CrateScope` is dirtied only when one of them moved.
    let cells = blaze_drive::Cells::NONE.with(blaze_drive::Kind::CrateScope, names::sections::cell_of);
    let mut req = Request::planted_in(config, ask, drive_plant, heap.alloc()).with_not_checked(|rule| rule == body_types::TYPES_NOT_CHECKED).with_cells(cells);
    // `run` checks only what its program reaches: the crate roots of its package's member
    // closure (blaze-drive run_scope.rs; run.md "End state (D113)", stage 4).
    if let Some(picked) = picked {
        req = req.with_member_closure(picked.units());
    }
    // Frames for the tasks and for records read back, made on first use (a repeat runs none).
    let mut frames: Option<work::Frames> = None;
    let mut high = work::HighWater::default();
    let mut failed = work::Failures::new_in(heap.alloc());
    // The command's handle on the one store: gets in place, puts into the lent's arena.
    // `false`: this machine is no trusted builder (CI's setting comes with publishing).
    let mut store = store::CommandStore::new(&cx, lent, false, open_ns, origin, phases.clock().clone(), err, alloc)?;
    // The workers' read side: today's tasks publish a file's outline and a manifest's units,
    // about one name a file, so 8 a file. A cold run starts with no files listed and opens them
    // as it goes: the table grows on a full one (`Settled::grown_in`), never stops. On the
    // global heap, named: a table outgrown is freed when the grown one replaces it. Over the
    // command's frame each stayed until the command ended, 45.8 MB of its 64 MiB on blaze's own
    // checkout (the profile's `effect publish` row, 2026-11-16).
    let mut table = Settled::new_in(files.len().saturating_mul(8), std::alloc::Global);
    // The effects the machine asks and the list they wait in: on the global heap, named, like
    // the machine that fills them (above). Each grows and empties many times a command; over the
    // command's frame every outgrown buffer stayed until the command ended (the refusal of
    // 2026-11-16: "the allocator refused 2850816 bytes ... for `Effect`").
    let mut effects: Vec<Effect<'_>, Alloc<'_>> = Vec::new_in(heap.alloc());
    let mut todo: VecDeque<Effect<'_>, Alloc<'_>> = VecDeque::new_in(heap.alloc());
    let mut prints: Vec<Print<'_>, &Frame> = Vec::new_in(frame);
    // The batches handed out and not yet run: at most one a worker (the request's own cap).
    let mut batches = std::vec::Vec::new_in(std::alloc::Global);
    // The opens asked and not yet probed.
    let mut opens: std::vec::Vec<(blaze_drive::Name, Vec<u8, Alloc<'_>>, bool), std::alloc::Global> = std::vec::Vec::new_in(std::alloc::Global);
    // The program's lowering refused it: its refusals are drawn with the request's findings.
    let mut refused = false;
    phases.mark_in("machine and store open", frame);
    let at = phases.now();
    let graph = facts.laid.as_ref().and_then(|laid| laid.graph.as_ref()).map(|graph| graph.source);
    phases.took("before the machine starts", frame);
    req.step(Event::Start { files: &files, gone: &gone, last, units, cold, graph }, &mut effects)?;
    phases.took(STEP, frame);
    phases.request.steps += phases.since(at);
    let mut started = 0;
    // The machine's answer once it came. Under the reached gate it answers at the program's exit
    // while work it asked for before is still queued: what writes (puts, prints, the table, the
    // last answer) is still done, and what would step the answered machine is not asked again.
    let mut answered: Option<Answer> = None;
    // The drive asked its `WriteLast`: this checkout's root names this answer.
    let mut wrote_last = false;
    let answer = loop {
        for effect in effects.drain(..) {
            todo.push_back(effect);
        }
        phases.took("the work list (effects moved to it)", frame);
        let Some(effect) = todo.pop_front() else {
            // Every open asked is probed together on the pool once nothing else is asked, then
            // answered in the order asked: each answer may ask a read, then a run.
            if !opens.is_empty() && answered.is_none() {
                let asked = core::mem::replace(&mut opens, std::vec::Vec::new_in(std::alloc::Global));
                let mut slots = std::vec::Vec::with_capacity_in(asked.len(), std::alloc::Global);
                for (name, path, module) in asked.iter() {
                    slots.push((*name, path.as_slice(), *module));
                }
                let at = phases.now();
                let probed = work::open_all(&cx, root, &facts.stamps, facts.workers, sizes.scratch, &slots)?;
                phases.took("opens probed", frame);
                for ((name, path, _), probed) in slots.iter().zip(probed) {
                    let slot = core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path a record names is not UTF-8" })?;
                    let id = sweep::opened(slot, probed?, &mut facts.rows);
                    if let Some(leaf) = facts.rows.rows.last() {
                        facts.found.note(slot, leaf.flags);
                    }
                    let stepped = phases.now();
                    phases.took("rows of the files opened", frame);
                    req.step(Event::Opened { name: *name, id }, &mut effects)?;
                    phases.took(STEP, frame);
                    phases.request.steps += phases.since(stepped);
                }
                phases.request.gets += phases.since(at);
                continue;
            }
            // Every batch handed out runs together once nothing else is asked (a read, an open):
            // the request hands out one a worker, and the pool is as wide as all of them.
            if !batches.is_empty() && answered.is_none() {
                let held = match frames {
                    Some(ref mut held) => held,
                    None => frames.insert(sizes.frames()?),
                };
                let ready = core::mem::replace(&mut batches, std::vec::Vec::new_in(std::alloc::Global));
                work::run_batches(&cx, root, &mut store, &mut facts, &mut req, ready, frame, held, &mut failed, &mut high, alloc, &mut effects, phases)?;
                phases.took("batches joined", frame);
                continue;
            }
            // A request that is out of effects without its answer is the machine's bug: named.
            break answered.ok_or(CliError::Drive(DriveError::Finished))?;
        };
        let at = phases.now();
        if answered.is_some()
            && matches!(effect, Effect::Get { .. } | Effect::Run { .. } | Effect::Open { .. } | Effect::StartProgram { .. })
        {
            continue;
        }
        let kind = match &effect {
            Effect::Get { .. } => "effect get",
            Effect::Put { .. } => "effect put",
            Effect::Run { .. } => "effect run (a batch queued)",
            Effect::Publish(_) | Effect::Unpublish(_) => "effect publish (the settled table)",
            Effect::Open { .. } => "effect open (queued)",
            Effect::StartProgram { .. } => "effect start program",
            Effect::Stop { .. } => "effect stop",
            Effect::Print(_) => "effect print",
            Effect::WriteLast { .. } => "effect write last",
            Effect::Done(_) => "effect done",
        };
        match effect {
            Effect::Get { key } => {
                // In place: the bytes are the lent's for the whole command (blaze-cas
                // plain/lent.rs); nothing is copied and no frame is reset.
                let held = store.get(&cx, key)?;
                let stepped = phases.now();
                phases.request.gets += phases.since(at);
                phases.took(kind, frame);
                req.step(Event::Got { key, bytes: held.map(|held| held.bytes) }, &mut effects)?;
                phases.took(STEP, frame);
                phases.request.steps += phases.since(stepped);
            }
            Effect::Put { key, bytes } => {
                if let Err(mut error) = store.put(&cx, key, &bytes) {
                    if let CliError::Store(crate::store::StoreFailure::TwoValues { clash, .. }) = &mut error {
                        clash.label_with(facts.names.as_ref());
                    }
                    return Err(error);
                }
                phases.request.puts += phases.since(at);
            }
            Effect::Run { batch } => batches.push(batch),
            // Puts are performed as they come, so every one before this is written.
            Effect::Publish(input) => {
                if table.publish(input).is_err() {
                    table = table.grown_in(std::alloc::Global)?;
                    table.publish(input)?;
                }
                phases.request.publish += phases.since(at);
            }
            Effect::Unpublish(name) => {
                if table.clear(name).is_err() {
                    table = table.grown_in(std::alloc::Global)?;
                    table.clear(name)?;
                }
                phases.request.publish += phases.since(at);
            }
            // A part names a file no row holds: open it where its record says (a crate root, or
            // a `mod x;` slot probed at `x.rs` and `x/mod.rs`), and record the read.
            Effect::Open { name, path, module } => opens.push((name, path, module)),
            // The walking skeleton's straight line, in this process and to its end before the
            // request goes on (tests/ladder/README.md); the design's route is blaze-run's Parent
            // taking the units' record and answering `Exited` when the child ends (drive.md, "Run").
            Effect::StartProgram { units, native } => {
                started = phases.since(began);
                // The program starts from the effect's keys: the units' record names its entry,
                // the `Native` record at `native` holds its image (never read back from `req`).
                // Its files are its package's `src/`, listed alone: never the checkout.
                // The start list's certificate and name: today's request certificate, and this
                // request shape's list (start_list.rs; run.md stage 7).
                let today = start_list::Today { certificate: start_list::Certificate(req.certificate().0.0), name: start_list::ListName::of(&shape) };
                // Where the program's names rows are, decided before it starts (run_names.rs): a
                // walk finds its crates now and writes them at the certificate; a hit reads that
                // record. A hit with none is dropped here, before the start, and asked in full.
                // The program `request` picked once (run_package.rs `pick`), handed down whole:
                // its root finds its crate, its package names the child (rule 77).
                let picked = picked.ok_or(CliError::RunNames(run_names::RunNamesError::NonePicked))?;
                watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Crates));
                let Some(found) = run_names::found(&cx, &req, picked.root, &mut store, &mut facts.bodies, heap, frame).map_err(CliError::RunNames)? else {
                    return Err(CliError::AnswerInFull { again: in_full });
                };
                // The body ids (run_ids.rs), also decided before the start: a walk builds them
                // from the answer, over the last answer's table, and writes them at the
                // certificate; a hit reads the certificate's table, or, with none, is dropped
                // here and asked in full. The parent takes them on its first function stepped.
                let ids_refused = |error: run_ids::RunIdsError<'static>| {
                    CliError::OwnProcess(own_process::OwnError::Parent(run_parent::ParentError::BodyIds { shown: Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) }))
                };
                watch.see(blaze_screen::Step::Began(blaze_screen::Stage::BodyIds));
                let made = match found.list() {
                    Some(list) => {
                        let previous = match last.map(blaze_drive::answers::read_last).transpose().map_err(|error| CliError::Drive(DriveError::LastFile(error)))? {
                            Some(l) => run_ids::RunIds::from_record(&cx, &mut store, l.cert, toolchain, &tools, alloc).map_err(ids_refused)?,
                            None => None,
                        };
                        run_ids::for_run(&cx, &req, toolchain, list, &tools, previous.as_ref(), &mut store, alloc).map_err(ids_refused)?
                    }
                    None => match run_ids::on_hit(&cx, &req, toolchain, &tools, &mut store, alloc).map_err(ids_refused)? {
                        run_ids::OnHit::Ids(ids) => ids,
                        run_ids::OnHit::AnswerInFull => return Err(CliError::AnswerInFull { again: in_full }),
                    },
                };
                let mut made = Some(made);
                // The front end the lazy parent makes on demand (run_parent.rs `CommandFront`):
                // each closure is called at most once, and only when a first call needs it. The
                // names records are made the one way `blaze check`'s are (run_names.rs).
                let (fetch_env, procs) = (&facts.fetch_env, facts.names.as_ref().and_then(|names| names.procs()));
                let (bodies, codes) = (&mut facts.bodies, &mut store);
                let mut names = || {
                    run_names::names_of(&cx, root, &found, picked.root, &mut *codes, &mut *bodies, fetch_env, procs, frame, heap)
                        .map_err(|error| run_parent::ParentError::Names { shown: Box::from(bstd::mem::format_in!(frame, "{error}").as_str()) })
                };
                let mut ids = || made.take().ok_or_else(|| run_parent::ParentError::BodyIds { shown: Box::from("the body ids were asked a second time (run_parent.rs `CommandFront` asks once)") });
                let ok = match program::run(&cx, root, launch, &picked, units, native, producers.get::<blaze_ids::producer::CodeBuilder>(), lent, today, &mut names, &mut ids, &mut phases.code_records, watch, heap, frame) {
                    Ok(status) => {
                        *ran = Some(status);
                        status == 0
                    }
                    Err(CliError::Program(RunError::Lower { .. })) => {
                        refused = true;
                        false
                    }
                    Err(CliError::Program(error)) => return Err(CliError::Ran { command: word, error }),
                    Err(error) => return Err(error),
                };
                let stepped = phases.now();
                phases.took(kind, frame);
                req.step(Event::Exited { ok }, &mut effects)?;
                phases.took(STEP, frame);
                phases.request.steps += phases.since(stepped);
            }
            // The in-process program has ended before any finding comes (above): nothing to stop.
            Effect::Stop { .. } => {}
            Effect::Print(finding) => prints.push(finding),
            Effect::WriteLast { bytes } => {
                // The leaf table rides in the same record, named by this checkout's root: the root
                // and every record it names are committed by the ONE flip after the answer is
                // printed (blaze-cas plain/root_commit_tests.rs), so no pointer names an absent record.
                let leaves = Leaves::bytes_in(&kept(&req, &followed, &facts.rows, frame), frame)?;
                store.set_last(&cx, &[&last_root, &seed_root], &seal::seal(&bytes, &leaves, frame)?)?;
                wrote_last = true;
                phases.request.write_last += phases.since(at);
            }
            Effect::Done(mut answer) => {
                phases.mark_in("request", frame);
                answer.profile.start_ns = started;
                answer.profile.verdict_ns = phases.since(began);
                answered = Some(answer);
            }
        }
        phases.took(kind, frame);
    };
    let mut answer = answer;
    // A seeded hit: its `Last` already names this certificate, so the drive asked no write. This
    // checkout's own root gets it, sealed with today's stamps, so its next command follows its
    // own table and hashes nothing its stamps prove. A root and the one sealed record; no
    // record of the drive's is put.
    if let Some(bytes) = seeded
        && !wrote_last
        && answer.profile.certified
    {
        let leaves = Leaves::bytes_in(&kept(&req, &followed, &facts.rows, frame), frame)?;
        store.set_last(&cx, &[&last_root], &seal::seal(bytes, &leaves, frame)?)?;
    }
    store.after_answer(&cx, &mut answer.profile)?;
    phases.mark_in("store after answer", frame);
    // A file read that was not the sweep's text (`Why::Changed`): the answer is refused, naming
    // each, after the store kept every other record this command settled (verified bytes).
    let mut changed = std::string::String::new();
    for fail in failed.iter().filter(|fail| matches!(fail.failure.why, blaze_drive::Why::Changed)) {
        let path = fail.path.unwrap_or("(a file with no path)");
        if !changed.lines().any(|line| line == path) {
            changed.push_str(path);
            changed.push('\n');
        }
    }
    if !changed.is_empty() {
        return Err(CliError::Changed { paths: changed });
    }
    let rows = kept(&req, &followed, &facts.rows, frame);
    let at = phases.now();
    // `blaze run` with findings: the program's refusals too, from its lowering alone.
    let program = match picked.filter(|_| ran.is_none() && (refused || !prints.is_empty())) {
        Some(picked) => Some(program_refusals(&cx, root, &picked, &rows, heap, frame)?),
        None => None,
    };
    // One file's findings at a time, in frames of a task's scratch size: freed a file and a
    // finding, and gone with the drawing.
    let mut scratch = print::Scratch::new(sizes.scratch)?;
    // The names step's note (what it resolved) is drawn there, with the other notes.
    draw(&cx, root, &req, command, &facts.found, &prints, program.as_ref(), &failed, facts.names.as_ref(), &notices, ran.is_some(), shown, &mut scratch, phases, frame, out)?;
    drop(scratch);
    phases.request.prints += phases.since(at);
    let checked = rows.of(LeafKind::Slot).filter(|leaf| !leaf.absent()).count();
    high.request = frame.peak();
    phases.worlds = u32::from(facts.names.is_some());
    let resolved = facts.names.as_ref().map_or(0, |names| names.resolved);
    // Whether the answer holds the crates' names. A walk forward from this request's own last
    // answer, and a certificate, stand on an answer that passed this test when it was made; a
    // whole walk (no last answer) lists every kind it holds in the profile's rows, so one with
    // source files and no `CrateScope` standing, found, made or fetched checked no names (no
    // dependency graph's job: a lock that is not there, or a layout read after `Start`).
    let scopes = answer.profile.kinds[blaze_drive::Kind::CrateScope.index()];
    let names_held = last.is_some() || answer.profile.certified || checked == 0 || scopes.standing + scopes.found + scopes.made + scopes.fetched > 0;    Ok(Finished { answer, checked: u32::try_from(checked).unwrap_or(u32::MAX), high, failed, resolved, names_held })
}

const LAST: &str = "a pointer in .blaze/v2/at (its last answer)";

/// The machine's own step in the profile's frame rows (`Phases::took`): what it pushed to the
/// effects list is counted here.
pub(crate) const STEP: &str = "machine step (the effects list)";

/// The program's files and every refusal of its lowering.
struct Refused<'f> {
    texts: Vec<(&'f str, &'f str), &'f Frame>,
    /// The refusals, and the advice the front end gave ([`RefusalRule::is_advice`]).
    ///
    /// [`RefusalRule::is_advice`]: blaze_ladder::refusal::RefusalRule::is_advice
    refusals: std::vec::Vec<Refusal, std::alloc::Global>,
    /// The one counted note about `blaze::` attributes recorded and checked by nothing.
    unchecked: Option<std::boxed::Box<str, std::alloc::Global>>,
}

/// The lowering's refusals of the files the answer holds, read at their real paths: what `blaze
/// run` would refuse to run, drawn beside the findings that kept it from running. The lowering's
/// tables live on `heap`, the process's, handed from `main`. The files are the program's
/// (`picked`: its crate root and that root's folder, run_package.rs).
fn program_refusals<'f>(cx: &Cx, root: &Dir<ReadWrite>, picked: &run_package::Program<'_>, rows: &Rows<'_>, heap: &'static blaze_own::Heap, frame: &'f Frame) -> Result<Refused<'f>, CliError> {
    let texts = program::sources(cx, root, picked.sources(), &[rows], frame)?;
    let mut files: Vec<SourceFile<'_>, &Frame> = Vec::with_capacity_in(texts.len(), frame);
    for &(path, text) in texts.iter() {
        files.push(SourceFile { path, text });
    }
    // What the runner refuses, every stage before placing: the same call `blaze run` stops at;
    // beside it, the advice about `blaze::` attributes it read and the count nothing checks.
    // With the setting `blaze run` builds with (D101): it decides which lang fns are needed.
    let overflow = program::dev_overflow(cx, root, frame)?;
    let (mut refusals, blaze) = blaze_ladder::run::findings(&files, picked.root, overflow, heap.alloc());
    let unchecked = blaze.note();
    refusals.extend(blaze.advice);
    Ok(Refused { texts, refusals, unchecked })
}

/// Every finding gathered, the request's and the program's together, in one order (decided
/// 2026-11-16): the manifests' first (a dependency not read explains what follows, and is the
/// one to fix first), then the sources' by path, each file's by position. A program's
/// refusal is still listed after a manifest's finding, never left out and never invented: the
/// runner's own front end makes it. After the findings, the notes ([`print::note`]).
#[expect(clippy::too_many_arguments, reason = "the drawing's inputs: each argument a different source of findings or of their text")]
fn draw<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    req: &Request<'_>,
    command: Command,
    found: &sweep::Found<'_>,
    prints: &[Print<'_>],
    program: Option<&Refused<'_>>,
    failed: &work::Failures<'_>,
    names: Option<&crate::names::Names<'_>>,
    notices: &[Notice],
    program_ran: bool,
    shown: &mut print::Shown<'f>,
    scratch: &mut print::Scratch,
    phases: &mut Phases,
    frame: &'f Frame,
    out: &Out,
) -> Result<(), CliError> {
    const NO_ROOM: CliError = CliError::Text { what: "the command's frame has no room to list its findings: a finding takes a `Listed`, its words and its path there" };
    // Each part's time, into the profile's `prints` line (phases.rs `PrintParts`).
    let mut lap = phases.now();
    let mut part = |phases: &mut Phases, into: fn(&mut phases::PrintParts) -> &mut u64| {
        let now = phases.now();
        *into(&mut phases.request.print_parts) += now.since(lap).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        lap = now;
    };
    // Each finding placed in today's text with its id and fix (fix.rs): a crate's unresolved
    // path carries its file, item and edition in its message, placed now in the file as it is
    // (names.rs `place_pieces`; rule 71). The id's item is a Rust file's outline item read in
    // one grammar for every file (fix.rs `ITEM_GRAMMAR`), so an id is a function of the text
    // alone. Where each is to be placed is said first, from the findings alone; then the files
    // are read, one at a time, in `scratch` (print.rs `list`): the command's frame keeps a
    // `Listed`, the words and a path a finding, never a file's text or a drawing.
    let first = shown.listed.len();
    // THE CALL SITE of `fix::checker_findings`, owed to the TYPES stage (work.rs CHECK_STAGES
    // "types"): when a body's `BodyCheck` record carries the checker's `suggests`, each of its
    // holes becomes a listed finding with its id and fix here, beside the prints below.
    let mut wanted: Vec<print::Wanted<'f>, &Frame> = Vec::try_with_capacity_in(prints.len(), frame).map_err(|_| NO_ROOM)?;
    let kept = |bytes: &[u8]| -> Result<&'f [u8], CliError> {
        let mut copy: Vec<u8, &'f Frame> = Vec::try_with_capacity_in(bytes.len(), frame).map_err(|_| NO_ROOM)?;
        copy.extend_from_slice(bytes);
        Ok(copy.leak())
    };
    // A consequence finding (print.rs `counted_only`, and a task that waited on a failed one) is
    // counted under its cause, never placed in its file, unless `--each` asks for each.
    let each = shown.each;
    let waited = |finding: &Print<'_>| finding.rule == work::WAITED && !each;
    let counted = |finding: &Print<'_>| print::counted_only(finding.rule) && !each;
    for finding in prints.iter() {
        if waited(finding) {
            // Its words are read from the print below (`waits`): nothing copied.
            wanted.push(print::Wanted { path: None, place: print::Place::Counted, words: &[] });
            continue;
        }
        if counted(finding) {
            // Only an item not checked is named with its file (not_checked/items.rs `keep`): a
            // task not run and a borrow check not done are counted by cause alone.
            let path = match finding.rule == body_types::TYPES_NOT_CHECKED {
                true => print::file_of(req, found, finding, frame)?,
                false => None,
            };
            wanted.push(print::Wanted { path, place: print::Place::Counted, words: kept(&finding.message)? });
            continue;
        }
        let placed = names::placed(finding.rule);
        let (path, place, words) = match placed && finding.file.is_none() {
            true => match names::place_pieces(&finding.message)? {
                (Some((path, chain, edition)), words) => {
                    // A crate's findings come sorted by file: the path is kept once a run.
                    let path = match wanted.last().and_then(|last: &print::Wanted<'f>| last.path) {
                        Some(last) if last == path => last,
                        _ => core::str::from_utf8(kept(path.as_bytes())?).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?,
                    };
                    (Some(path), print::Place::Chain(chain, edition), words)
                }
                (None, words) => (None, print::Place::At(finding.place), words),
            },
            false => (print::file_of(req, found, finding, frame)?, print::Place::At(finding.place), &finding.message[..]),
        };
        wanted.push(print::Wanted { path, place, words: kept(words)? });
    }
    print::list(cx, root, prints, &wanted, scratch, frame, &mut shown.listed)?;
    fix::lengths(&mut shown.listed[first..], frame);
    part(phases, |p| &mut p.listing);
    // (file, position, the request's finding or the program's refusal, its index).
    let mut order: Vec<(Option<&str>, u32, bool, usize), &Frame> = Vec::try_with_capacity_in(prints.len() + program.map_or(0, |program| program.refusals.len()), frame).map_err(|_| NO_ROOM)?;
    // The waited tasks (not_checked/waited.rs), and the tasks that failed on their own: each
    // ROOT drawn last, once, with what hangs from it counted under it.
    let mut waits: Vec<not_checked::waited::Wait<'_>, &Frame> = Vec::new_in(frame);
    let mut roots: Vec<usize, &Frame> = Vec::new_in(frame);
    for (i, finding) in prints.iter().enumerate() {
        // Counted under its root whether or not `--each` draws it too: the summary line counts
        // every item not checked (not_checked/items.rs `summary`).
        if finding.rule == work::WAITED {
            // Under each name whose record held this one wait (blaze-drive `Print::also`: a
            // library's scope and its test unit's, one label): a task waiting on the other
            // waits through it to the same root. Under `name` alone, what waited on the other
            // printed as a cause of its own, "`CrateScope(4509d8507735)` failed: 10 crates ..",
            // a key and no reason (tempo, 2026-11-17 08:02).
            let (on, label) = not_checked::waited::read(&finding.message)?;
            for name in core::iter::once(finding.name).chain(finding.also.iter().copied()) {
                waits.push(not_checked::waited::Wait { name, on, label, file: finding.file });
            }
            if waited(finding) {
                continue;
            }
        }
        if counted(finding) {
            // Kept by cause (not_checked/items.rs, tasks.rs) and counted; nothing drawn.
            let listed = shown.listed[first + i];
            print::finding(cx, &listed, &[], shown, scratch.one(), out)?;
            continue;
        }
        // A file not text is the user's error, not a failure (print.rs counts it so), but what
        // waits on it hangs from it the same way.
        if finding.rule == work::TASK_FAILED || finding.rule == work::FILE_NOT_TEXT {
            roots.push(first + i);
            continue;
        }
        // A finding whose item is no longer in its file is listed at its name, with no file.
        let listed = &shown.listed[first + i];
        let at = if listed.path.is_some() { listed.start } else { finding.place.saturating_add(finding.start) };
        order.push((listed.path, at, false, i));
    }
    let groups = not_checked::waited::groups(&mut waits, frame)?;
    part(phases, |p| &mut p.grouping);
    // The notes first, the code's findings after them, the failures last: what to fix is what
    // the eye lands on, above the closing line.
    // A failed task is the tool's, not the code's: named with its file, and the command fails
    // (its answer is not written, so the next command runs the task again). What was not
    // checked because of it is named below by cause, at crate grain (not_checked.rs, D115 (6)).
    work::failed_notes(failed, scratch.one(), &mut |line| print::note(cx, line, shown, frame, out))?;
    // Over the drive's whole list: the machine's own `Waited` failures (a part whose home
    // failed) are in it and not in `failed`.
    not_checked::lines(req.failures(), failed, names, frame, &mut |line| print::note(cx, line, shown, frame, out))?;
    for notice in notices {
        let mut words = bstd::mem::String::new_in(frame);
        core::fmt::write(&mut words, format_args!("{notice}")).map_err(|_| CliError::Text { what: "a note's words" })?;
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    // What the answer covers, said once when it is less than the command is defined to cover
    // (rule 61): derived from the kinds this worker builds and the runner does, so it shrinks
    // as kinds are built. For `run` it is also what a refusal's program was run in, and only
    // then: a `run` that reports nothing says nothing, its program's output the whole output.
    let reported = !prints.is_empty() || program.is_some() || !failed.is_empty() || !notices.is_empty();
    let said = match command {
        Command::Run => reported,
        Command::Check | Command::Test => true,
    };
    if let Some(words) = left_out_note(command, frame)?.filter(|_| said) {
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    if let Some(unchecked) = program.and_then(|program| program.unchecked.as_deref()) {
        print::note(cx, unchecked, shown, frame, out)?;
    }
    // Nothing follows a program that ran (`close`): its output is the command's, and a note
    // about the check's known gaps is `check`'s to say, not a clean run's.
    if program_ran {
        // Only the findings, as before; no note of what was not checked.
    } else {
        // No note of the names step's own counts ("names resolved for N crates; M paths
        // unresolved"): it is said only when that step ran, so a no-op repeat, which runs
        // nothing, printed other bytes than the run it repeats. What is printed is what the
        // records hold (the findings, the causes), the same bytes for the same tree.
        // The tasks not run because an input they read could not be made, one line a cause
        // (not_checked/tasks.rs): the input's own finding is printed once, below.
        not_checked::tasks::lines(&shown.not_run, frame, &mut |line| print::note(cx, line, shown, frame, out))?;
        // The items `check` could not check, one line a cause (not_checked/items.rs).
        if matches!(command, Command::Check | Command::Run) {
            not_checked::items::lines(&shown.unchecked, frame, &mut |line| print::note(cx, line, shown, frame, out))?;
        }
    }
    part(phases, |p| &mut p.notes);
    if let Some(program) = program {
        for (i, refusal) in program.refusals.iter().enumerate() {
            let path = refusal.file.and_then(|file| program.texts.get(file as usize)).map(|it| it.0);
            order.push((path, refusal.place.saturating_add(refusal.start), true, i));
        }
    }
    let source = |path: Option<&str>| !path.is_some_and(|path| path == "Cargo.toml" || path.ends_with("/Cargo.toml"));
    // In a file, its lint findings after its type and borrow findings (lint.md: advice after).
    let lint = |entry: &(Option<&str>, u32, bool, usize)| !entry.2 && lints::is_lint(prints[entry.3].rule);
    order.sort_by(|a, b| (source(a.0), a.0, lint(a), a.1, a.2).cmp(&(source(b.0), b.0, lint(b), b.1, b.2)));
    // A file's findings are neighbours in that order: the file is read once for the run.
    let mut at = 0;
    while at < order.len() {
        let (path, _, of_program, i) = order[at];
        if of_program {
            // Only a program's refusals are entered so, above.
            if let Some(program) = program {
                print::refusal(cx, &program.refusals[i], &program.texts, shown, scratch.one(), out)?;
            }
            at += 1;
            continue;
        }
        let run = &order[at..at + order[at..].iter().take_while(|next| !next.2 && next.0 == path).count()];
        at += run.len();
        print::file_findings(cx, root, path, run.iter().map(|&(_, _, _, i)| first + i), shown, scratch, out)?;
    }
    part(phases, |p| &mut p.findings);
    // Last, each task that failed on its own, in name order (rule 25), drawn once with what
    // hangs from it on the line under it.
    roots.sort_by(|a, b| (shown.listed[*a].about, shown.listed[*a].path, shown.listed[*a].start).cmp(&(shown.listed[*b].about, shown.listed[*b].path, shown.listed[*b].start)));
    let mut said: Vec<bool, &Frame> = Vec::with_capacity_in(groups.len(), frame);
    said.extend(groups.iter().map(|_| false));
    for (n, &at) in roots.iter().enumerate() {
        let listed = shown.listed[at];
        print::file_findings(cx, root, listed.path, core::iter::once(at), shown, scratch, out)?;
        // A root twice (two configurations of one task) has its count under the last.
        let last_of_name = roots.get(n + 1).is_none_or(|next| shown.listed[*next].about != listed.about);
        if !last_of_name {
            continue;
        }
        // What waited on it, or on a name whose record held this same failure, printed once
        // here (blaze-drive `Print::also`: a library and its test unit): one line, the counts
        // of all of them together.
        let mut causes: Vec<usize, &Frame> = Vec::new_in(frame);
        for cause in core::iter::once(&listed.about).chain(prints[at - first].also.iter()) {
            if let Ok(g) = groups.binary_search_by(|group| group.cause.cmp(cause))
                && !said[g]
            {
                said[g] = true;
                causes.push(g);
            }
        }
        match causes.as_slice() {
            [] => {}
            [one] => {
                shown.behind.add(groups[*one].items);
                print::note(cx, not_checked::waited::line(&groups[*one], scratch.one())?.as_str(), shown, frame, out)?;
            }
            many => {
                let merged = not_checked::waited::merged(many.iter().map(|g| &groups[*g]), frame)?;
                shown.behind.add(merged.items);
                print::note(cx, not_checked::waited::line(&merged, scratch.one())?.as_str(), shown, frame, out)?;
            }
        }
    }
    // A cause with no failure of its own printed (it was not filed: the machine's own, or a
    // hole): named by the drive's name, with its count.
    for (group, _) in groups.iter().zip(said.iter()).filter(|(_, said)| !**said) {
        shown.behind.add(group.items);
        let one = scratch.one();
        let words = bstd::mem::format_in!(one, "`{}` failed: {}", group.cause, not_checked::waited::line(group, one)?.as_str());
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    part(phases, |p| &mut p.roots);
    Ok(())
}

/// The note of what `command` was defined to cover and this build left out: `None` when it
/// covered all. "this build of blaze does not resolve names, expand macros or check types yet:
/// `check` read the manifests and parsed".
fn left_out_note(command: Command, frame: &Frame) -> Result<Option<bstd::mem::String<&Frame>>, CliError> {
    // `check`'s closing line names what it did not check, from `work::CHECK_STAGES`, the one
    // list: a second note would say it twice, in other words.
    if command == Command::Check {
        return Ok(None);
    }
    let mut left: Vec<&str, &Frame> = Vec::new_in(frame);
    left.extend(work::left_out(command));
    if left.is_empty() {
        return Ok(None);
    }
    let mut words = bstd::mem::String::new_in(frame);
    words.push_str("this build of blaze does not ");
    list(&mut words, &left, "or");
    // Each stage once, in the command's order: what the worker did (a part of a stage named
    // as the part, `work::does`), then what the runner did.
    let stages = |pick: &dyn Fn(blaze_drive::Kind) -> bool, said: &dyn Fn(blaze_drive::Kind) -> &'static str| {
        let mut picked: Vec<&str, &Frame> = Vec::new_in(frame);
        for kind in command.needs().iter().filter(|kind| pick(**kind)) {
            if picked.scan_for(&said(*kind)).is_none() {
                picked.push(said(*kind));
            }
        }
        picked
    };
    let built = stages(&|kind| work::builds(kind), &work::does);
    let ran = stages(&|kind| !work::whole(kind) && work::runner_does(command, kind), &|kind| kind.stage());
    words.push_str(" yet");
    let word = match command {
        Command::Check => "check",
        Command::Run => "run",
        Command::Test => "test",
    };
    let wrote = |words: &mut bstd::mem::String<&Frame>, args: core::fmt::Arguments<'_>| {
        core::fmt::write(words, args).map_err(|_| CliError::Text { what: "a note's words" })
    };
    if !built.is_empty() {
        wrote(&mut words, format_args!(": `{word}` did "))?;
        list(&mut words, &built, "and");
    }
    if !ran.is_empty() {
        words.push_str(if built.is_empty() { ": its program runner did " } else { ", and its program runner did " });
        list(&mut words, &ran, "and");
        words.push_str(" in the subset of Rust its front end reads so far");
    }
    Ok(Some(words))
}

/// `a, b and c`.
fn list(words: &mut bstd::mem::String<&Frame>, items: &[&str], last: &str) {
    for (i, item) in items.iter().enumerate() {
        match i {
            0 => {}
            _ if i + 1 == items.len() => {
                words.push_str(" ");
                words.push_str(last);
                words.push_str(" ");
            }
            _ => words.push_str(", "),
        }
        words.push_str(item);
    }
}


/// A note about the command, drawn after its findings: an old home rebuilt, or a torn one.
enum Notice {
    Older(home::Older),
    Torn(seal::Torn),
}

impl core::fmt::Display for Notice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Notice::Older(older) => older.fmt(f),
            Notice::Torn(torn) => torn.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use blaze_drive::{TaskFailure, Why};
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

    struct Checked {
        failed: std::vec::Vec<(Option<std::string::String>, TaskFailure)>,
        runs: u32,
        certified: bool,
        checked: u32,
        high: work::HighWater,
        /// The `BodyCheck` tasks answered, and the file jobs that answered them (one a home
        /// file a round of batches).
        bodies: u64,
        body_jobs: u64,
        /// The crates whose names the command resolved.
        resolved: u32,
        /// Its profile's lines, each kind's counts: what a failing count names.
        profile: std::string::String,
        /// The errors it found (`Counts.errors`): notes are not counted.
        errors: u32,
        /// The tasks that failed on their own (`Counts.failures`), apart from the errors.
        failures: u32,
        /// The exit status `command` returns for it (`close`), `None` where `close` failed.
        status: Option<u8>,
        /// `--fix` refused (its line printed) and wrote nothing.
        refused: bool,
        /// Each finding's (code, id), in the order listed.
        ids: std::vec::Vec<(std::string::String, std::string::String)>,
        /// Each finding's words, in the same order: what a failing assertion quotes.
        words: std::vec::Vec<std::string::String>,
    }

    impl Checked {
        fn refused() -> Checked {
            Checked {
                failed: std::vec::Vec::new(),
                runs: 0,
                certified: false,
                checked: 0,
                high: work::HighWater::default(),
                bodies: 0,
                body_jobs: 0,
                resolved: 0,
                profile: std::string::String::new(),
                errors: 0,
                failures: 0,
                status: Some(1),
                refused: true,
                ids: std::vec::Vec::new(),
                words: std::vec::Vec::new(),
            }
        }
    }

    mod check_whole_tests;
    mod draw_tests;
    mod extra_roots_tests;
    mod failed_call_tests;
    mod failed_roots_tests;
    mod fix_tests;
    mod members_tests;
    mod names_records_tests;
    mod engine_check_tests;
    mod native_image_tests;
    mod include_data_tests;
    mod include_outside_tests;
    mod lints_tests;
    mod order_tests;
    mod owners_tests;
    mod pooled_scope_tests;
    mod refused_targets_tests;
    mod run_scope_tests;
    mod signature_grain_tests;
    mod stale_facts_tests;

    fn write(io: &Io, path: &str, text: &str) {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
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
        let (id, files) = blaze_world::fixture::shelf(
            graph::TRIPLE,
            blaze_world::fixture::STD,
            SHELF_CFG,
            blaze_own::testing::heap().alloc(),
        );
        for (name, bytes) in files {
            let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("a path");
            io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
        }
        id
    }

    /// A file's bodies are checked by one job a round of batches: the file is read and outlined
    /// once for all of them, not once a body (rule 77; on blaze's own checkout the per-body
    /// outline was the command's 120 CPU seconds). Without the grouping (`run_batches` making
    /// a job a body) the jobs equal the bodies and this fails.
    #[test]
    fn a_files_bodies_are_checked_by_one_job_a_round() {
        let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        let mut lib = std::string::String::new();
        for i in 0..400 {
            writeln!(lib, "pub fn f{i}() -> u32 {{ {i} }}").expect("a string takes text");
        }
        write(&io, "src/lib.rs", &lib);
        let first = check(&io, work::Sizes::USUAL);
        assert!(first.failed.is_empty(), "{:?}", first.failed);
        assert_eq!(first.bodies, 400, "each body is a task of its own");
        assert!(first.body_jobs >= 1 && first.body_jobs * 8 <= first.bodies, "{} file jobs for {} bodies of one file", first.body_jobs, first.bodies);
    }

    fn check(io: &Io, sizes: work::Sizes) -> Checked {
        ask(io, (Command::Check, "check"), sizes).expect("the request answers")
    }

    fn ask(io: &Io, command: (Command, &'static str), sizes: work::Sizes) -> Result<Checked, CliError> {
        ask_view::<fix::Atomic>(io, command, sizes, View::Person, None)
    }

    /// The command as `command` runs it, in `view` (its lines drawn as off a terminal), with
    /// `--fix` when `fix` gives its selectors, the fixed files written through `W`.
    fn ask_view<W: fix::Writer + Default>(io: &Io, command: (Command, &'static str), sizes: work::Sizes, view: View, fix: Option<&[&str]>) -> Result<Checked, CliError> {
        ask_framed::<W>(io, command, sizes, view, fix, TASK_FRAME)
    }

    /// [`ask_view`] with the command's own frame of `command_frame` bytes (a task's, as `main`
    /// gives it, but for a test of what the command keeps there).
    std::thread_local! {
        /// The workers a test's commands are given (`--workers`): 1 unless a test sets it.
        static WORKERS: core::cell::Cell<u32> = const { core::cell::Cell::new(1) };
    }

    fn ask_framed<W: fix::Writer + Default>(io: &Io, command: (Command, &'static str), sizes: work::Sizes, view: View, fix: Option<&[&str]>, command_frame: usize) -> Result<Checked, CliError> {
        let cx = io.cx(BUDGET, command_frame).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        let (out, err) = (io.need_out().expect("out"), io.need_err().expect("err"));
        let mut phases = Phases::new(io.need_clock().expect("a clock"));
        let shown = print::Shown::new(blaze_report::frame::Caps::LINES, view, "/ws", frame);
        let mut ran = None;
        let launch = program::Launch { io, select: run_package::Select::NONE, grants: &[], args: &[] };
        let said = words(command.0, &out, &err);
        let producers = match sizes.plant {
            work::Plant::Producer(row, older) => {
                blaze_ids::producer::Producers::for_tests(b"test").planted_for_tests(row, older.as_bytes())
            }
            _ => blaze_ids::producer::Producers::for_tests(b"test"),
        };
        let toolchain = shelf(io);
        let opened = store::open_for_command(&cx, io, Some("/h"), &root, "ws", phases.clock(), &err)?;
        let mut run = |mut shown, ran: &mut Option<u8>, phases: &mut Phases| {
            let workers = WORKERS.with(core::cell::Cell::get);
            let finished = request(&cx, &root, ("ws", None, None, Some("/h"), None, Some("/"), toolchain, crate::sdk::NONE_IN_TESTS, Err(fetch::ExeDirWhy::NotRead)), None, (&opened.home, &opened.lent, opened.open_ns), command, workers, &producers, sizes, &launch, &blaze_screen::Unwatched, frame, said, &err, ran, &mut shown, phases, blaze_own::testing::heap(), false);
            finished.map(|finished| (finished, shown))
        };
        let (finished, shown) = match fix {
            None => run(shown, &mut ran, &mut phases)?,
            Some(selectors) => match fix_then_check::<W>(&cx, &root, selectors, (blaze_report::frame::Caps::LINES, view, "/ws"), frame, said, &mut |shown| run(shown, &mut ran, &mut phases))? {
                Fixed::Refused => return Ok(Checked::refused()),
                Fixed::Checked(finished, shown) => (finished, shown),
            },
        };
        // The drive's count holds the parts it failed `Waited` itself (no task ran for them).
        assert!(finished.answer.failed as usize >= finished.failed.len());
        // The closing line and the status, as `command` makes them; then the store's one commit.
        let status = close(&cx, command.0, &finished, &shown, ran, 0, frame, said).ok().map(|status| status.0);
        store::after_answer_printed(&cx, opened.lent, false, phases.clock(), &err)?;
        Ok(Checked {
            status,
            // The causes: a task failed only because one it needed failed is not one.
            failed: finished
                .failed
                .iter()
                .filter(|f| !matches!(f.failure.why, Why::Waited { .. }))
                .map(|f| (f.path.map(std::string::String::from), f.failure))
                .collect(),
            runs: finished.answer.profile.runs(),
            certified: finished.answer.profile.certified,
            checked: finished.checked,
            high: finished.high,
            // The `BodyCheck` tasks made, from the profile: work.rs's `BODIES` row also times
            // each `Signature` job (`row_of`) and each entry again with handed inputs (a body
            // first answers its `Need`s, D114's amendment), so its count is no body count.
            bodies: u64::from(made(&finished.answer.profile.to_string(), "BodyCheck")),
            body_jobs: phases.times(work::BODY_JOBS),
            resolved: finished.resolved,
            profile: finished.answer.profile.to_string(),
            // The errors as the closing line counts them (`close`): a note (a stage that did not
            // run, its why) is listed but is no error.
            errors: shown.counts.errors,
            failures: shown.counts.failures,
            refused: false,
            ids: shown.listed.iter().map(|l| (l.code().to_owned(), l.id.as_str().to_owned())).collect(),
            words: shown.listed.iter().map(|l| std::string::String::from_utf8_lossy(l.words).into_owned()).collect(),
        })
    }

    /// A module file looked for and not found is an absence the crate's names read: creating
    /// it reruns the crate, and the error the missing file made is gone. Control: a names
    /// record that lists no absence keeps standing, and the error with it.
    #[test]
    fn creating_a_missing_module_file_clears_its_error() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "mod x;\npub use x::f;\n");
        let missing = check(&io, work::Sizes::USUAL);
        assert!(missing.errors > 0, "a missing module file is an error:\n{}", missing.profile);
        write(&io, "src/x.rs", "pub fn f() {}\n");
        let created = check(&io, work::Sizes::USUAL);
        assert!(created.failed.is_empty(), "{:?}", created.failed);
        assert_eq!(created.errors, 0, "the created file clears the error: findings {:?} {:?}\n{}", created.ids, created.words, created.profile);
        // Nor a note: a fresh home on these files checks the bodies against the unit graph (the
        // members' own lock), so the edited home must too (rule 26), never say it has no graph.
        assert!(created.words.is_empty(), "the edited home lists what a fresh one does not: {:?}\n{}", created.words, created.profile);
    }

    /// An answer with errors is certified: a task's typed failure is filed as its record with
    /// the inputs it was handed as its reads, so the repeat stands on its certificate with the
    /// same errors, and fixing the cause asks again and clears them. Here member `a`'s root is
    /// not UTF-8, so its names fail (filed by names.rs with the files it read) and `b`, which
    /// depends on it, fails `Waited` on `a` (filed by work.rs): `b`'s key holds none of `a`'s
    /// text, so only its read of `a` asks it again. `Err` names the step that broke.
    fn a_filed_failure_stands_until_its_input_moves(plant: work::Plant) -> Result<(), std::string::String> {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
        write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
        write(&io, "b/src/lib.rs", "pub fn h() -> u8 {\n    a::f()\n}\n");
        {
            let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
            root.publish(&cx, &Path::new("a/src/lib.rs").expect("a path"), Content::bytes(b"pub fn f() -> u8 {\n    1\n}\n// \xff\xfe\n"), Durable::No).expect("write");
        }
        let sizes = work::Sizes { plant, ..work::Sizes::USUAL };
        // A file blaze cannot read as text is the user's error at that file (work.rs
        // `FILE_NOT_TEXT`), never only a failure of the tool.
        let first = check(&io, sizes);
        if first.errors == 0 {
            return Err(format!("a module file that is not UTF-8 is no error:\n{}", first.profile));
        }
        let repeat = check(&io, sizes);
        if !(repeat.certified && repeat.runs == 0 && (repeat.errors, repeat.failures) == (first.errors, first.failures)) {
            return Err(format!("the repeat: certified {}, {} runs, {} errors {} failures (first {} {}):\n{}", repeat.certified, repeat.runs, repeat.errors, repeat.failures, first.errors, first.failures, repeat.profile));
        }
        write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
        let fixed = check(&io, sizes);
        // A waited task's record left standing is no error and no failure, but its finding is
        // still listed (counted under a cause): a stale one is a filed failure that outlived
        // its input.
        let waited = fixed.ids.iter().filter(|(code, _)| code == "waited").count();
        if fixed.errors + fixed.failures != 0 || !fixed.failed.is_empty() || waited != 0 {
            return Err(format!("after the fix: {} errors {} failures, failed {:?}:\n{}", fixed.errors, fixed.failures, fixed.failed, fixed.profile));
        }
        Ok(())
    }

    #[test]
    fn an_answer_with_errors_is_certified_and_asked_again_when_its_input_moves() {
        if let Err(broke) = a_filed_failure_stands_until_its_input_moves(work::Plant::None) {
            panic!("{broke}");
        }
        // Control: a filed failure without its reads stands after the fix, and is caught.
        assert!(a_filed_failure_stands_until_its_input_moves(work::Plant::FiledWithoutReads).is_err(), "the planted bug (a filed failure with no reads) went uncaught");
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
    fn edits_answer_as_a_fresh_home(tape: &blaze_sim::Tape, plant: blaze_drive::Plant) -> Result<(), blaze_sim::Broke> {
        let sizes = work::Sizes { plant: work::Plant::Drive(plant), ..work::Sizes::USUAL };
        let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
        let mut files: std::vec::Vec<(&'static str, std::string::String)> = std::vec::Vec::new();
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "mod x;\npub use x::f;\n");
        files.push(("Cargo.toml", std::string::String::from(MANIFEST)));
        files.push(("src/lib.rs", std::string::String::from("mod x;\npub use x::f;\n")));
        let ask_now = |io: &Io, sizes: work::Sizes| -> Result<(Checked, std::vec::Vec<std::string::String>), blaze_sim::Broke> {
            let before = stdout(io).len();
            let checked = ask(io, (Command::Check, "check"), sizes).map_err(|e| blaze_sim::Broke::Oracle(format!("the request failed: {e:?}")))?;
            let lines = findings_since(io, before);
            Ok((checked, lines))
        };
        ask_now(&io, sizes)?;
        for step in 0..tape.range(1..6, "steps") {
            edit_step(tape, &io, &mut files)?;
            let (home, home_lines) = ask_now(&io, sizes)?;
            let fresh_io = Io::simulated(SimWorld::new(3, Limits::USUAL));
            for (path, text) in files.iter() {
                write(&fresh_io, path, text);
            }
            let (fresh, fresh_lines) = ask_now(&fresh_io, work::Sizes::USUAL)?;
            // Every finding's words too, notes included: `errors` counts no note, and a note one
            // home gives and a fresh one does not is the same history dependence (rule 26).
            if (home.errors, home.status, &home_lines, &home.words) != (fresh.errors, fresh.status, &fresh_lines, &fresh.words) {
                let tree: std::vec::Vec<&str> = files.iter().map(|(p, _)| *p).collect();
                return Err(blaze_sim::Broke::Oracle(format!(
                    "differs from a fresh home at step {step} on {tree:?}: fresh {:?} {:?} {:?} {fresh_lines:?}, home {:?} {:?} {:?} {home_lines:?}; the files {files:?}\nFRESH\n{}\nHOME\n{}",
                    fresh.errors, fresh.status, fresh.words, home.errors, home.status, home.words, fresh.profile, home.profile
                )));
            }
            let (again, _) = ask_now(&io, sizes)?;
            if again.runs != 0 {
                return Err(blaze_sim::Broke::Oracle(format!("an unchanged repeat ran {} tasks at step {step}:\n{}", again.runs, again.profile)));
            }
        }
        Ok(())
    }

    #[test]
    fn edits_of_module_files_answer_as_a_fresh_home() {
        blaze_sim::check(blaze_sim::Config::cases(40), |tape| edits_answer_as_a_fresh_home(tape, blaze_drive::Plant::None));
    }

    // OPEN BUG, found by these at 400 cases (seed 245, BLAZE_SIM_TAPE='0 2 0 2 0 6 0 3': create
    // `x/mod.rs`, edit lib.rs, delete `x/mod.rs`): the request fails `NoFile` for FileItems of
    // `src/x.rs` (drive `absent_module`: no `part_paths` row). The controls run 200 cases until
    // it is fixed (drive.md, RUNNER 8); raise them back to 400 with the fix.
    #[test]
    fn the_fresh_home_oracle_catches_forgotten_absent_readers() {
        blaze_sim::control(blaze_sim::Config::cases(200), blaze_drive::Plant::None, blaze_drive::Plant::ForgetAbsentReaders, "differs from a fresh home", edits_answer_as_a_fresh_home);
    }

    #[test]
    fn the_zero_runs_clause_catches_absent_reads_handed_as_hints() {
        blaze_sim::control(blaze_sim::Config::cases(200), blaze_drive::Plant::None, blaze_drive::Plant::HintAbsentReads, "an unchanged repeat ran", edits_answer_as_a_fresh_home);
    }

    /// A missing module file is one finding at its `mod` line, naming both places; the rest of
    /// the crate is still checked (the request answers, nothing fails).
    #[test]
    fn a_missing_module_file_is_one_finding_at_its_mod_line() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() {}\nmod x;\n");
        let missing = check(&io, work::Sizes::USUAL);
        assert!(missing.failed.is_empty(), "{:?}", missing.failed);
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
        let broken = check(&io, work::Sizes::USUAL);
        assert!(broken.failed.is_empty(), "{:?}", broken.failed);
        assert_eq!((broken.errors, broken.status), (1, Some(1)), "stdout {:?}", stdout(&io));
        let out = stdout(&io);
        assert!(out.contains("lib.rs:3:12"), "{out}");
    }

    /// A `use` of nothing in the project's own crate is one error at the failing segment's
    /// line and column, exit 1; its record holds the place relative to the `use` item, so a body
    /// edit above it moves the printed line with the file and reruns no names; the import fixed,
    /// no error is left.
    #[test]
    fn a_bad_import_is_one_error_at_its_segment() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\nuse crate::nope::Thing;\n");
        let bad = check(&io, work::Sizes::USUAL);
        assert!(bad.failed.is_empty(), "{:?}", bad.failed);
        let out = stdout(&io);
        assert_eq!((bad.errors, bad.status), (1, Some(1)), "{out}\n{}", bad.profile);
        assert!(out.contains("lib.rs:2:12: error[unresolved]: no `nope` in `crate`"), "{out}");
        let seen = out.len();
        write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\nuse crate::nope::Thing;\n");
        let moved = check(&io, work::Sizes::USUAL);
        let out = stdout(&io);
        assert_eq!((moved.errors, moved.status), (1, Some(1)), "{out}\n{}", moved.profile);
        assert!(out[seen..].contains("lib.rs:4:12: error[unresolved]: no `nope` in `crate`"), "{}", &out[seen..]);
        assert_eq!(made(&moved.profile, "CrateScope"), 0, "a body edit reruns no names:\n{}", moved.profile);
        write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\nmod nope { pub struct Thing; }\nuse crate::nope::Thing;\n");
        let fixed = check(&io, work::Sizes::USUAL);
        assert_eq!((fixed.errors, fixed.status), (0, Some(3)), "{}", &stdout(&io)[seen..]);
    }

    /// The `made` count of `kind`'s profile row.
    fn made(profile: &str, kind: &str) -> u32 {
        let row = profile.lines().find(|line| line.split_whitespace().next() == Some(kind)).unwrap_or("");
        let words: std::vec::Vec<&str> = row.split_whitespace().collect();
        words.iter().position(|word| *word == "made").and_then(|at| words.get(at + 1)).and_then(|n| n.parse().ok()).unwrap_or(0)
    }

    /// One part per body, keyed by its item's text: an edit inside one body re-parses that body
    /// alone, while the other stands.
    #[test]
    fn an_edit_of_one_body_reparses_that_body_alone() {
        let io = Io::simulated(SimWorld::new(8, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\npub fn g() -> u8 { 2 }\n");
        let first = check(&io, work::Sizes::USUAL);
        assert_eq!((first.status, made(&first.profile, "BodyCheck")), (Some(3), 2), "{}", first.profile);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\npub fn g() -> u8 { 3 }\n");
        let edited = check(&io, work::Sizes::USUAL);
        assert_eq!((edited.status, made(&edited.profile, "BodyCheck")), (Some(3), 1), "{}", edited.profile);
    }

    /// Piece 7 (body_types.rs): a body's types are checked in its `BodyCheck` task over the
    /// crates' records. `fn f() -> u32 { "x" }` is one `types` error (the Reference, "Function
    /// body": the block's type must be the declared return type), `g` beside it clean; the
    /// repeat makes no `BodyCheck`; a signature edit of `g` in the same file re-checks `g`'s
    /// body alone (its key is its item's text; `f` reads no head of `g`, D114). Without the arm
    /// (work.rs `run_batches` calling `prepare`) no type error is found and the status is 3.
    /// Prints `body types: cold 1 error, repeat made 0, signature edit made 1`.
    #[test]
    fn a_body_type_error_is_found_over_records_and_kept() {
        let io = Io::simulated(SimWorld::new(8, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let cold = check(&io, work::Sizes::USUAL);
        let out = stdout(&io);
        assert_eq!((cold.errors, cold.status), (1, Some(1)), "{out}\n{}", cold.profile);
        assert!(cold.ids.iter().any(|(code, _)| code == "types"), "{:?}\n{out}", cold.ids);
        let repeat = check(&io, work::Sizes::USUAL);
        assert_eq!((repeat.errors, made(&repeat.profile, "BodyCheck")), (1, 0), "{}", repeat.profile);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u64 { 1 }\n");
        let edited = check(&io, work::Sizes::USUAL);
        assert_eq!(made(&edited.profile, "BodyCheck"), 1, "{}", edited.profile);
        std::println!("body types: cold {} error, repeat made {}, signature edit made {}", cold.errors, made(&repeat.profile, "BodyCheck"), made(&edited.profile, "BodyCheck"));
    }

    /// A body's record made by an older body checker is never read after a checker fix
    /// (`Row::BodyChecker` of the producers' table, in the tool, D105): the same text, checked by the older producer
    /// and then by today's, makes every body again; today's twice makes none. Without the
    /// producer in the key (home.rs `tool` leaving the table out) the second check makes 0 and the
    /// older answer is served.
    #[test]
    fn a_body_record_of_an_older_checker_is_made_again() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let older = work::Sizes {
            plant: work::Plant::Producer(blaze_ids::producer::Row::BodyChecker, "an older body checker"),
            ..work::Sizes::USUAL
        };
        let by_older = check(&io, older);
        assert_eq!(made(&by_older.profile, "BodyCheck"), 2, "{}", by_older.profile);
        let today = check(&io, work::Sizes::USUAL);
        assert_eq!(made(&today.profile, "BodyCheck"), 2, "an older checker's body records were read\n{}", today.profile);
        let again = check(&io, work::Sizes::USUAL);
        assert_eq!(made(&again.profile, "BodyCheck"), 0, "{}", again.profile);
    }

    /// Each kind's key holds its own producer (D105, home.rs `tools`): a names solver built from
    /// other code makes the scopes again and leaves every body record standing (its key holds the
    /// body checker's row, which did not move, and the scopes it read came out the same). The
    /// control (rule 23): the body checker's row moved instead makes every body again. Without
    /// the split (every kind keyed by the whole tool, as before piece 3) the first half makes 2.
    #[test]
    fn a_moved_names_producer_leaves_body_records_standing() {
        let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u32 { \"x\" }\npub fn g() -> u32 { 1 }\n");
        let today = check(&io, work::Sizes::USUAL);
        assert_eq!(made(&today.profile, "BodyCheck"), 2, "{}", today.profile);
        let names = work::Sizes {
            plant: work::Plant::Producer(blaze_ids::producer::Row::Names, "an older names solver"),
            ..work::Sizes::USUAL
        };
        let by_names = check(&io, names);
        let scopes = ["CrateScope", "ModuleScope", "Group"].map(|kind| made(&by_names.profile, kind)).iter().sum::<u32>();
        assert!(scopes > 0, "a moved names solver made no scope again\n{}", by_names.profile);
        assert_eq!(made(&by_names.profile, "BodyCheck"), 0, "a moved names solver made bodies again\n{}", by_names.profile);
        let checker = work::Sizes {
            plant: work::Plant::Producer(blaze_ids::producer::Row::BodyChecker, "an older body checker"),
            ..work::Sizes::USUAL
        };
        let by_checker = check(&io, checker);
        assert_eq!(made(&by_checker.profile, "BodyCheck"), 2, "the control: a moved body checker\n{}", by_checker.profile);
        std::println!("producers: a moved names solver made {scopes} scopes and 0 bodies; a moved checker made 2 bodies");
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
        let clean = check(&io, work::Sizes::USUAL);
        assert_eq!(clean.status, Some(3), "{}", clean.profile);
        let out = stdout(&io);
        let last = out.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
        assert!(last.contains("incomplete"), "{out}");
        // A narrow line keeps what was NOT checked and drops the rest (blaze-report `closing`).
        // Its bodies' types are checked over records (piece 7): only borrows are left out.
        assert!(last.contains("NOT checked: borrows") && !last.contains("types") && !last.contains(" ok"), "{last}");
        // A "NOT checked" never stands without its reason: no body was refused here, so the
        // stage's own (work.rs `CHECK_STAGES`) is a note before the closing line. Control: it
        // names the flag that keeps the stage off, so it goes when the flag flips.
        let reason = out.lines().find(|line| line.contains("NOT checked: borrows:")).unwrap_or_else(|| panic!("no note says why borrows were not checked:\n{out}"));
        assert!(reason.contains("BORROWS_ARE_ERRORS"), "{reason}");
        let io = Io::simulated(SimWorld::new(6, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\nfn fn\n");
        let broken = check(&io, work::Sizes::USUAL);
        assert!(broken.status == Some(1) && broken.errors >= 1, "status {:?}, {} errors", broken.status, broken.errors);
        assert!(stdout(&io).contains("error"), "{}", stdout(&io));
        let io = Io::simulated(SimWorld::new(6, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let said = explain(&cx, Some("3"), &io.need_out().expect("out")).expect("explain 3");
        assert_eq!(said.0, 0);
        assert!(stdout(&io).starts_with("exit status 3: check: incomplete"), "{}", stdout(&io));
    }

    /// The closing note says what the command left out, from the kinds the worker builds: each
    /// stage once, and none at all once every kind is built (checked by its derivation: a kind
    /// the worker builds is never named, one it does not always is).
    #[test]
    fn the_note_names_each_stage_left_out_once() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        // `check`'s closing line is the one place that says what it left out.
        assert!(left_out_note(Command::Check, frame).expect("words").is_none());
        let words = left_out_note(Command::Run, frame).expect("words").expect("run leaves stages out today");
        assert_eq!(
            words.as_str(),
            // Derived from the kinds the worker builds (`work::left_out`): `Signature` ("check
            // types") and the native link are built now, so `run` names them as done.
            "this build of blaze does not expand macros yet: `run` did read the manifests, fetch the dependencies' sources, run build scripts, parse, check types, check the bodies' syntax and types and link native code, and its program runner did resolve names, check types and generate code in the subset of Rust its front end reads so far"
        );
        for command in [Command::Check, Command::Run, Command::Test] {
            let left: std::vec::Vec<&str> = work::left_out(command).collect();
            // A stage is named exactly when one of its kinds the command needs is not covered.
            for kind in command.needs() {
                let short = command.needs().iter().any(|k| k.stage() == kind.stage() && !work::covers(command, *k));
                assert_eq!(left.contains(&kind.stage()), short, "{:?} {:?}", command, kind);
            }
            let mut unique = left.clone();
            unique.dedup();
            assert_eq!(unique, left, "a stage named twice");
        }
    }

    /// `blaze run` after `blaze check` of a package with an error (a dependency not read): the
    /// run's certificate is its own, so it walks forward from the check's answer, and answers
    /// with the error, never stepping the machine past its answer (hello-world, 2026-11-16:
    /// "a step after the request answered"). The package is a library, so the error is
    /// [`CliError::RunPackage`] naming it.
    #[test]
    fn run_after_check_answers_with_the_error() {
        let io = workspace(10);
        write(&io, "Cargo.toml", &format!("{MANIFEST}\n[dependencies]\nfar = \"1\"\n"));
        let checked = check(&io, SMALL);
        assert!(checked.failed.is_empty());
        let ran = ask(&io, (Command::Run, "run"), SMALL);
        // The package is a library (src/lib.rs, no `[[bin]]`): the run answers that there is no
        // program, by the package's name, before reading a source file; never the front end's
        // `NoMain` (2026-11-16), and never the old front end's lowering refusal, which was an
        // accident of what it could not read (the test passed on it before the switch).
        match ran {
            Err(CliError::RunPackage(no @ run_package::PickError::NoBinary { .. })) => {
                assert_eq!(no, run_package::PickError::NoBinary { package: "wide".into() });
                let shown = no.to_string();
                assert!(shown.contains("`wide` has no binary target"), "{shown}");
            }
            other => panic!("wanted NoBinary naming `wide`, got {:?}", other.err()),
        }
    }

    // A clean `blaze run` saying nothing of its own on either stream is tests/run_output.rs
    // `a_clean_run_prints_only_its_program`: the program runs by the launcher in its own
    // process (own_process.rs), which a simulated host under `cargo test` has not beside it.

    /// The store keeps one pack a command and merges them past its cap: after more edits than
    /// the cap, each a pack, every record still answers (a repeat stands with no run, and an
    /// undo to the first tree finds that tree's certificate in the merged pack).
    #[test]
    fn records_outlive_the_packs_merge() {
        let io = workspace(10);
        let cold = check(&io, SMALL);
        for edit in 0..12 {
            write(&io, "src/m3.rs", &format!("{}fn more{edit}() {{}}\n", functions(100)));
            let edited = check(&io, SMALL);
            assert!(edited.failed.is_empty() && edited.runs >= 1, "edit {edit}: {} runs", edited.runs);
        }
        let again = check(&io, SMALL);
        assert!(again.certified && again.runs == 0, "a repeat after the merge: certified {}, {} runs:\n{}", again.certified, again.runs, again.profile);
        write(&io, "src/m3.rs", &functions(100));
        let undone = check(&io, SMALL);
        assert!(undone.certified && undone.runs == 0 && undone.checked == cold.checked, "the undo: {} runs", undone.runs);
    }

    /// An undo back to a certified tree answers from its certificate and keeps every file in
    /// the leaf table, the undone one too: the next request (a manifest edit, which re-makes
    /// the units that list the crate root) finds each file a part names. Before the fix the
    /// undone file left the table and that request stopped with `NoFile` (tempo, 2026-11-16).
    #[test]
    fn an_undo_keeps_every_file_for_the_next_edit() {
        let io = workspace(10);
        let cold = check(&io, SMALL);
        write(&io, "src/m3.rs", &format!("{}fn more() {{}}\n", functions(100)));
        let edited = check(&io, SMALL);
        assert_eq!(edited.checked, cold.checked);
        write(&io, "src/m3.rs", &functions(100));
        let undone = check(&io, SMALL);
        assert!(undone.certified, "the cold tree's certificate answers the undo");
        assert_eq!(undone.checked, cold.checked, "every file kept on the undo");
        write(&io, "Cargo.toml", &format!("{MANIFEST}\n[dependencies]\nfar = \"1\"\n"));
        let after = ask(&io, (Command::Check, "check"), SMALL);
        assert!(after.as_ref().is_ok_and(|after| after.failed.is_empty() && after.checked == cold.checked), "{:?}", after.err());
    }

    const SMALL: work::Sizes = work::Sizes { scratch: SCRATCH, batch: TASK_FRAME, plant: work::Plant::None };

    /// Rule 11: a task's work lives in its own frame, reset after it, so a workspace whose
    /// outlines together are many times one task's frame checks, and the scratch never holds
    /// more than one file's.
    #[test]
    fn a_workspace_larger_than_one_task_frame_checks() {
        let io = workspace(100);
        let first = check(&io, SMALL);
        assert!(first.failed.is_empty(), "{:?}", first.failed);
        assert_eq!(first.checked as usize, MODULES + 2);
        assert!(first.high.scratch <= SCRATCH, "{:?}", first.high);
        let again = check(&io, SMALL);
        assert!(again.certified && again.runs == 0, "a repeat: certified {}, {} runs", again.certified, again.runs);
    }

    /// Rule 20: a file whose outline its task's frame refuses fails that one task, named with
    /// its file and the frame's size; every other record is written (the next command, with
    /// the file fixed, runs that one task alone), and no answer is written over the hole.
    #[test]
    fn an_oversize_file_fails_its_task_alone_and_names_the_file() {
        let io = workspace(3_000);
        let first = check(&io, SMALL);
        let [(path, failure)] = first.failed.as_slice() else { panic!("one failure: {:?}", first.failed) };
        assert_eq!(path.as_deref(), Some("src/big.rs"));
        assert!(matches!(failure.why, Why::Panicked { limit, used } if limit == SCRATCH as u64 && used <= limit), "{failure:?}");
        write(&io, "src/big.rs", &functions(10));
        let fixed = check(&io, SMALL);
        assert!(fixed.failed.is_empty(), "{:?}", fixed.failed);
        // As in `a_planted_panic_fails_one_task`: outline, surface, the names of the library and
        // of its test unit, and every body of the crate: the first run checked none, its names
        // having failed `Waited` on the oversize file (piece 7, 2026-11-16).
        assert_eq!(made(&first.profile, "BodyCheck"), 0, "no body is checked over names that failed:\n{}", first.profile);
        assert_eq!(
            (made(&fixed.profile, "FileItems"), made(&fixed.profile, "CrateScope"), made(&fixed.profile, "BodyCheck")),
            (1, 2, u32::try_from(MODULES * 100 + 10).expect("a small count")),
            "only the failed file's task runs again, its new surface and its two crates' names, and the bodies first checked now:\n{}",
            fixed.profile
        );
        let again = check(&io, SMALL);
        assert!(again.certified && again.runs == 0);
    }

    /// A body edit reruns the file's outline and no `Surface`: the surface's source is what
    /// name resolution reads of each entry (blaze-file-items names_digest.rs: its name,
    /// visibility, attributes, form), never a body. A parameter added to a function leaves it
    /// equal too (2026-11-16: the surface hashed signatures until then, and this edit reran the
    /// surface and the crate's names: 5 runs; "an edit to a parameter's, a field's or a return
    /// type [..] leaves a file's surface equal and its crate's names are not solved again").
    /// A new item moves that one `Surface`, and the names of the library and of its test unit
    /// (a crate of its own since `check` took the test targets) are solved again. Control: a
    /// surface that hashed bodies would make the first edit's runs more than 2.
    #[test]
    fn a_body_edit_reruns_no_surface_and_a_signature_edit_one() {
        let io = workspace(10);
        let first = check(&io, SMALL);
        assert!(first.failed.is_empty(), "{:?}", first.failed);
        write(&io, "src/m3.rs", &functions(100).replace("fn f00003() {}", "fn f00003() { let _x = 1; }"));
        let body = check(&io, SMALL);
        assert!(body.failed.is_empty(), "{:?}", body.failed);
        assert_eq!((body.runs, made(&body.profile, "BodyCheck")), (2, 1), "a body edit runs its file's outline and that one body:\n{}", body.profile);
        write(&io, "src/m3.rs", &functions(100).replace("fn f00003() {}", "fn f00003(_x: u8) { let _x = 1; }"));
        let signature = check(&io, SMALL);
        // At signature grain (D114): the outline, the edited head's `Signature` and its own
        // body (its text is its key). No other body reads `f00003`'s head, so none runs; until
        // D114 every body of the file ran (101, 0, 0, 100), their keys holding the file's text
        // around the bodies.
        assert_eq!(
            (
                signature.runs,
                made(&signature.profile, "Surface"),
                made(&signature.profile, "CrateScope"),
                made(&signature.profile, "Signature"),
                made(&signature.profile, "BodyCheck")
            ),
            (3, 0, 0, 1, 1),
            "a parameter edit runs its file's outline, that head and that one body:\n{}",
            signature.profile
        );
        let added = functions(100).replace("fn f00003() {}", "fn f00003(_x: u8) { let _x = 1; }") + "pub fn added() {}\n";
        write(&io, "src/m3.rs", &added);
        let item = check(&io, SMALL);
        // Made: the outline, the surface, the two crates' `CrateScope`, every head of the file
        // (each `Signature`'s source holds its file's surface) and every body of the file (each
        // reads its file's `Surface` whole, COARSE(D114 e)). Entries are those makes and the
        // `Need`s answered on the way: since the Owners body side (2026-11-16 21:23) a body
        // first answers one `Need` of its owners' crates when they moved (body_types.rs
        // `prepare`), so the entries were 109 against 105 makes with "BodyCheck .. needs 4" in
        // the profile, not a body made twice. How many `Need`s the schedule answers is its
        // order's, so the entries are checked against the makes and needs the profile lists.
        let makes = (
            made(&item.profile, "FileItems"),
            made(&item.profile, "Surface"),
            made(&item.profile, "CrateScope"),
            made(&item.profile, "Signature"),
            made(&item.profile, "BodyCheck"),
        );
        assert_eq!(makes, (1, 1, 2, 101, 101), "a new item makes its file's outline, its surface, its two crates' names, its file's heads and its file's bodies:\n{}", item.profile);
        let needs = needs(&item.profile);
        assert_eq!(
            item.runs,
            makes.0 + makes.1 + makes.2 + makes.3 + makes.4 + needs,
            "every entry is a make or a `Need` answered (no body made twice):\n{}",
            item.profile
        );
        std::println!("a new item: made {makes:?}, needs {needs}, entries {}", item.runs);
    }

    /// The `needs` counts of every kind's profile row, summed: the entries that answered `Need`.
    fn needs(profile: &str) -> u32 {
        let mut sum = 0;
        for line in profile.lines() {
            let words: std::vec::Vec<&str> = line.split_whitespace().collect();
            if !words.contains(&"entries") {
                continue;
            }
            let at = words.iter().position(|word| *word == "needs").and_then(|at| words.get(at + 1)).and_then(|n| n.parse::<u32>().ok());
            sum += at.unwrap_or(0);
        }
        sum
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
        let cold = check(&io, work::Sizes::USUAL);
        assert!(cold.failed.is_empty(), "{:?}", cold.failed);
        assert!(cold.resolved > 1, "cold resolves the toolchain's crates and the binary: {}", cold.resolved);
        write(&io, "src/main.rs", "fn main() { let _x = 1; }\n");
        let body = check(&io, work::Sizes::USUAL);
        assert_eq!(body.resolved, 0, "a body edit resolves no crate's names");
        write(&io, "src/main.rs", "fn main() { let _x = 1; }\npub fn extra(_x: u8) {}\n");
        let signature = check(&io, work::Sizes::USUAL);
        assert_eq!(signature.resolved, 2, "an added `pub fn` reruns the names of the binary and of its test unit alone");
    }

    /// Rule 20 for a bug: a task that panics fails alone; the rest of the request goes on.
    #[test]
    fn a_planted_panic_fails_one_task() {
        let io = workspace(10);
        let planted = work::Sizes { plant: work::Plant::PanicIn("src/m3.rs"), ..SMALL };
        let first = check(&io, planted);
        let [(path, failure)] = first.failed.as_slice() else { panic!("one failure: {:?}", first.failed) };
        assert_eq!(path.as_deref(), Some("src/m3.rs"));
        assert!(matches!(failure.why, Why::Panicked { .. }), "{failure:?}");
        // The panicked file's outline failed, so its crate's names (and its test unit's) failed
        // `Waited`, and so did every body of the crate: a body's types read its crate's names
        // (body_types.rs), so the first run checked none (since piece 7, 2026-11-16).
        assert_eq!(made(&first.profile, "BodyCheck"), 0, "no body is checked over names that failed:\n{}", first.profile);
        let fixed = check(&io, SMALL);
        assert!(fixed.failed.is_empty());
        // The file's outline, its surface, and the names of the library and of its test unit,
        // failed `Waited` last time, are made fresh; and every body of the crate (40 files of
        // 100 and `big.rs`'s 10), never checked before. No other file's outline is made again.
        assert_eq!(
            (made(&fixed.profile, "BodyCheck"), made(&fixed.profile, "FileItems"), made(&fixed.profile, "CrateScope")),
            (u32::try_from(MODULES * 100 + 10).expect("a small count"), 1, 2),
            "only the panicked file's task runs again, its surface and its two crates' names, never made, and the bodies first checked now:\n{}",
            fixed.profile
        );
    }

    /// A `check` after another command (here `test`, another shape; a new build of blaze is
    /// another tool) answers as a first `check` does: the pointer the other command left is no
    /// answer of this one's, so the layout is read and the dependency graph's job is handed to
    /// the machine. On blaze's own checkout (2026-11-16) a new build's `check` over the old
    /// build's pointer ran no `UnitGraph`, `Fetch` or `CrateScope` and printed "0 errors".
    /// Without `walks_from` in `request` the last assertion fails: the bad import is not found.
    #[test]
    fn a_check_after_another_command_resolves_the_names() {
        let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", "pub fn f() -> u8 { 1 }\nuse crate::nope::Thing;\n");
        let tested = ask(&io, (Command::Test, "test"), work::Sizes::USUAL).expect("the request answers");
        assert_eq!(tested.errors, 1, "{}", tested.profile);
        let seen = stdout(&io).len();
        let checked = check(&io, work::Sizes::USUAL);
        let out = stdout(&io);
        assert!(checked.resolved > 0 || made(&checked.profile, "CrateScope") > 0 || checked.profile.lines().any(|line| line.starts_with("CrateScope")), "no crate's names in the answer:\n{}", checked.profile);
        assert_eq!((checked.errors, checked.status), (1, Some(1)), "{}\n{}", &out[seen..], checked.profile);
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
        let hollow = check(&io, work::Sizes::USUAL);
        let out = stdout(&io);
        assert!(hollow.failed.is_empty(), "{:?}", hollow.failed);
        assert!(out.contains("NOT checked: names, types, borrows"), "{out}\n{}", hollow.profile);
        // `f` is an item left unchecked (its crate has no unit graph), so the status is
        // `UNCHECKED_ITEMS` (4), which `close` puts before `INCOMPLETE` (3).
        assert_eq!(hollow.status, Some(4), "{out}");
        let seen = out.len();
        write(&io, "Cargo.toml", "[package]\nname = \"lockless\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        let whole = check(&io, work::Sizes::USUAL);
        let out = stdout(&io);
        assert!(whole.failed.is_empty(), "{:?}", whole.failed);
        // The body's record of the first run said "no unit graph" and read the root's units:
        // the manifest edit moves them, so the body is checked again, not left standing with
        // the old note (rule 21; the read in body_types.rs `TypesJob::units`).
        assert!(out[seen..].contains("NOT checked: borrows") && !out[seen..].contains("NOT checked: names") && !out[seen..].contains("types"), "{}\n{}", &out[seen..], whole.profile);
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
        let whole = match ask(&io, (Command::Check, "check"), work::Sizes::USUAL) {
            Ok(whole) => whole,
            Err(error) => panic!("the request stops: {error}"),
        };
        assert!(whole.failed.is_empty(), "{:?}", whole.failed);
        assert_eq!((made(&whole.profile, "FileItems"), made(&whole.profile, "BodyCheck")), (2, 3), "{}\n{}", stdout(&io), whole.profile);
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
        let whole = match ask(&io, (Command::Check, "check"), work::Sizes::USUAL) {
            Ok(whole) => whole,
            Err(error) => panic!("the request stops: {error}"),
        };
        assert!(whole.failed.is_empty(), "{:?}", whole.failed);
        assert_eq!((whole.errors, made(&whole.profile, "FileItems"), made(&whole.profile, "CrateScope") > 0), (0, 2, true), "{}\n{}", stdout(&io), whole.profile);
    }
}
