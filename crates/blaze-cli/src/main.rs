//! `blaze`, on the new item layer (docs/v2/plan/design/drive.md, "The system side"): one command
//! is one blaze-drive [`Request`], and this binary performs its effects through bstd and nothing
//! else. `blaze check [dir]`, `blaze run [dir]`, `blaze test [dir]`; `[dir]` is the checkout,
//! relative to where blaze was started, in any spelling a person writes ([`checkout`]). The start
//! directory itself (`.`, or no argument) is refused until bstd gives a `Dir` for it.
//!
//! | module | what |
//! |---|---|
//! | [`sweep`] | the checkout's files and their content ids, behind the `Sweeper` trait |
//! | [`store`] | records by key, behind the `Store` trait |
//! | [`work`] | the tasks: `Units`, `FileItems`; the rest answer what is not built |
//! | [`print`] | findings as `file:line:col`, and the profile |

#![feature(macro_attr, allocator_api)]
#![forbid(unsafe_code)]

mod checkout;
mod phases;
mod print;
mod program;
mod store;
mod sweep;
mod work;

use core::fmt;

use blaze_drive::{
    Answer, Ask, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Request, Settled, Space, TableFull, Tool,
    WireError,
};
use blaze_own::{Alloc, Vec, VecDeque};
use bstd::fs::{Dir, FsError, ReadWrite};
use bstd::io::Missing;
use bstd::mem::{Bytes, Frame};
use bstd::out::{Out, OutError};
use bstd::path::{Path, PathError};
use bstd::entry::Status;
use bstd::{Cx, Io, entry::Error};

use crate::checkout::Checkout;
use crate::phases::Phases;
use crate::store::{FileStore, Store};
use crate::sweep::{FromMeta, Leaf, Leaves, StatSweep, Sweeper};

/// Why a command stopped: each names its thing (rule 19).
#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    /// A command this binary does not do yet.
    NotBuilt { command: &'static str },
    Missing(Missing),
    Path(PathError),
    Fs(FsError),
    Out(OutError),
    Drive(DriveError),
    /// The checkout's leaf table is not one.
    Leaves(WireError),
    /// A manifest line the units reader does not take.
    Manifest { line: u32, why: &'static str },
    /// A file the outline cannot read: not UTF-8, or past 4 GiB.
    Text { what: &'static str },
    /// The command answered with findings.
    Findings(u32),
    /// The workers' settled table was sized below the run.
    Table(TableFull),
    /// `blaze run`'s program did not run: the stage and the function it stopped at.
    Program(blaze_ladder::run::RunError),
    /// The checkout is the start directory (`.`, or no argument), which bstd gives no `Dir` for.
    Here,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(what) => write!(f, "usage: blaze <check|run|test> [dir]: {what}"),
            CliError::NotBuilt { command } => write!(f, "`blaze {command}` is not built on the new item layer yet (drive.md)"),
            CliError::Missing(missing) => write!(f, "{missing}"),
            CliError::Path(error) => write!(f, "{error}"),
            CliError::Fs(error) => write!(f, "{error}"),
            CliError::Out(error) => write!(f, "{error}"),
            CliError::Drive(error) => write!(f, "{error}"),
            CliError::Leaves(error) => write!(f, "the checkout's leaf table: {error}; remove .blaze/v2/leaves to start over"),
            CliError::Manifest { line, why } => write!(f, "Cargo.toml:{line}: {why} (drive.md: the units reader takes a single package)"),
            CliError::Text { what } => write!(f, "{what}"),
            CliError::Findings(n) => write!(f, "{n} findings"),
            CliError::Table(full) => write!(f, "{full}"),
            CliError::Program(error) => write!(f, "blaze run: {error}"),
            CliError::Here => write!(
                f,
                "the checkout is the start directory, and bstd has no `Dir` for it yet (a `FileName` is never empty): run blaze from the folder above and name the checkout"
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

from!(Missing(Missing), Path(PathError), Fs(FsError), Out(OutError), Drive(DriveError), Table(TableFull));

/// `blaze run` and `test` fail on any finding, lints included (bottom.md 11); `check` reports.
fn every_rule(_: u16) -> bool {
    true
}

/// The tool's identity: what every key starts from. A new binary's records are not an old one's.
fn tool() -> Tool {
    Tool(blaze_ids::ContentId::of("blaze-cli tool 1", env_version().as_bytes()))
}

fn env_version() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"), " drive 1")
}

/// The status: 0 for a check that passed; `blaze run`'s, the status its program's `main` ended with.
#[bstd::main]
fn main(cx: &Cx, io: Io) -> Result<Status, Error> {
    let mut phases = Phases::new(io.need_clock()?);
    let out = io.need_out()?;
    let err = io.need_err()?;
    phases.mark("output handles");
    // A failure is printed in one place, bstd's exit path (`error: ` and its message).
    command(cx, &io, &out, &err, &mut phases).map_err(Error::from)
}

fn command(cx: &Cx, io: &Io, out: &Out, err: &Out, phases: &mut Phases) -> Result<Status, CliError> {
    let args = io.need_args()?;
    let command = match args.get(1) {
        Some("check") => Command::Check,
        Some("run") => Command::Run,
        Some("test") => Command::Test,
        Some("build") => return Err(CliError::NotBuilt { command: "build" }),
        Some("export") => return Err(CliError::NotBuilt { command: "export" }),
        Some(_) | None => return Err(CliError::Usage("the command is check, run or test")),
    };
    phases.mark("argument list");
    let frame = cx.frame();
    // The frame's first allocation takes its first chunk from blaze-mem: its own phase, so the
    // line says what that costs apart from the arguments' parse.
    drop(Vec::<u8, &Frame>::with_capacity_in(1, frame));
    phases.first_chunk();
    let dir = match checkout::checkout(args.get(2), frame)? {
        Checkout::Under(dir) => dir,
        Checkout::Here => return Err(CliError::Here),
    };
    phases.mark("arguments");
    let fs = io.need_fs()?;
    let root = fs.open_dir(cx, &dir)?;
    let workers = io.need_host()?.cores().get();
    phases.mark("open checkout");
    let mut ran = None;
    let mut answer = request(cx, &root, command, u32::try_from(workers).unwrap_or(u32::MAX), frame, out, &mut ran, phases)?;
    answer.profile.coordinator_ns = phases.request.steps;
    let p = &answer.profile;
    err.line(
        cx,
        format_args!(
            "blaze: {} runs, {} store reads ({} found), certificate {}; {:.3} ms in main; .blaze/v2/profile.txt",
            p.runs(),
            p.gets,
            p.hits,
            if p.certified { "stood" } else { "missed" },
            phases.in_main() as f64 / 1e6,
        ),
    )?;
    phases.mark("answer printed");
    // The answer is printed: the profile is written after it, in place, never synced.
    print::profile(cx, &root, &answer, phases, frame)?;
    if answer.findings > 0 {
        return Err(CliError::Findings(answer.findings));
    }
    match command {
        Command::Check => Ok(Status::OK),
        // The program ran at the machine's `StartProgram`: its status is the command's.
        Command::Run => ran.map(Status).ok_or(CliError::Drive(DriveError::Finished)),
        Command::Test => Err(CliError::NotBuilt { command: "test" }),
    }
}

const LAST: &str = ".blaze/v2/last";

/// One request, its effects performed in the order they come: gets and puts at once, task runs
/// one after another on this thread (the pool is the next piece: `workers` is what the machine
/// emits to).
/// `ran` is the status of the program `blaze run` started, once it ended.
fn request(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    command: Command,
    workers: u32,
    frame: &Frame,
    out: &Out,
    ran: &mut Option<u8>,
    phases: &mut Phases,
) -> Result<Answer, CliError> {
    let cx = cx.span("blaze request");
    let began = phases.now();
    let leaves = Leaves::read(&cx, root, frame)?;
    phases.mark("leaf table");
    let mut swept = Vec::new_in(frame);
    StatSweep { stamps: FromMeta, plant: sweep::Plant::None }.sweep(&cx, root, &leaves, frame, &mut swept)?;
    phases.mark("sweep");
    let mut files: Vec<FileEntry<'_>, &Frame> = Vec::with_capacity_in(swept.len(), frame);
    for leaf in swept.iter() {
        files.push(FileEntry {
            path: leaf.path.as_bytes(),
            id: leaf.id,
            changed: leaves.get(leaf.path.as_bytes()) != Some(leaf.id),
            manifest: sweep::is_manifest(leaf.path),
        });
    }
    let mut gone: Vec<&[u8], &Frame> = Vec::new_in(frame);
    leaves.gone(&files, &mut gone);
    let mut last_bytes = Bytes::new_in(frame, "the last answer");
    let last_path = Path::new(LAST)?;
    let last = match root.stat(&cx, &last_path)? {
        Some(_) => {
            root.read_all(&cx, &last_path, &mut last_bytes)?;
            Some(last_bytes.as_slice())
        }
        None => None,
    };
    phases.mark("last answer read");
    let config = Config { tool: tool(), workers, most_entries: 64, most_jobs: 1 << 24, is_error: every_rule };
    // The machine allocates through a one-word handle; the frame carries no header of its own,
    // so it is erased into one here, on this stack, for this request.
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut req = Request::new_in(config, Ask { command, args: b"", gate: Gate::Beside, space: Space::NONE }, alloc);
    let mut store = FileStore::open(root)?;
    // The workers' read side: two slots a name at most a half full; about 128 names a file
    // (items, bodies, scopes) is the sizing until the profile says otherwise.
    let table = Settled::new_in(files.len().saturating_mul(128), frame);
    let mut effects: Vec<Effect<'_>, Alloc<'_>> = Vec::new_in(alloc);
    let mut todo: VecDeque<Effect<'_>, Alloc<'_>> = VecDeque::new_in(alloc);
    phases.mark("machine and store open");
    let at = phases.now();
    req.step(Event::Start { files: &files, gone: &gone, last }, &mut effects)?;
    phases.request.steps += phases.since(at);
    let mut started = 0;
    loop {
        for effect in effects.drain(..) {
            todo.push_back(effect);
        }
        let Some(effect) = todo.pop_front() else {
            // A request that is out of effects without its answer is the machine's bug: named.
            return Err(CliError::Drive(DriveError::Finished));
        };
        let at = phases.now();
        match effect {
            Effect::Get { key } => {
                let mut bytes = Bytes::new_in(frame, "a record");
                let found = store.get_into(&cx, key, &mut bytes)?;
                let bytes = found.then_some(bytes.as_slice());
                let stepped = phases.now();
                phases.request.gets += phases.since(at);
                req.step(Event::Got { key, bytes }, &mut effects)?;
                phases.request.steps += phases.since(stepped);
            }
            Effect::Put { key, bytes } => {
                store.put(&cx, key, &bytes)?;
                phases.request.puts += phases.since(at);
            }
            Effect::Run { batch } => {
                work::run_batch(&cx, root, &files, &mut req, batch, frame, &mut effects, phases)?;
            }
            // Puts are performed as they come, so every one before this is written.
            Effect::Publish(input) => {
                table.publish(input)?;
                phases.request.publish += phases.since(at);
            }
            Effect::Unpublish(name) => {
                table.clear(name)?;
                phases.request.publish += phases.since(at);
            }
            // The walking skeleton's straight line, in this process and to its end before the
            // request goes on (tests/ladder/README.md); the design's route is blaze-run's Parent
            // taking the units' record and answering `Exited` when the child ends (drive.md, "Run").
            Effect::StartProgram { .. } => {
                started = phases.since(began);
                let status = program::run(&cx, root, frame)?;
                *ran = Some(status);
                let stepped = phases.now();
                req.step(Event::Exited { ok: status == 0 }, &mut effects)?;
                phases.request.steps += phases.since(stepped);
            }
            Effect::Stop { errors } => return Err(CliError::Findings(errors)),
            Effect::Print(finding) => {
                print::finding(&cx, root, &req, &finding, frame, out)?;
                phases.request.prints += phases.since(at);
            }
            Effect::WriteLast { bytes } => {
                root.publish(&cx, &last_path, bstd::fs::Content::bytes(&bytes), bstd::fs::Durable::No)?;
                leaves_after(&cx, root, &swept, frame)?;
                phases.request.write_last += phases.since(at);
            }
            Effect::Done(mut answer) => {
                phases.mark("request");
                answer.profile.start_ns = started;
                answer.profile.verdict_ns = phases.since(began);
                store.after_answer(&cx, &mut answer.profile)?;
                phases.mark("store after answer");
                return Ok(answer);
            }
        }
    }
}

/// The leaf table goes after `last`: a crash between them reports more files changed, never fewer.
fn leaves_after(cx: &Cx, root: &Dir<ReadWrite>, swept: &[Leaf<'_>], frame: &Frame) -> Result<(), CliError> {
    Leaves::write(cx, root, swept, frame)
}
