//! The command's driver of one crate's C tasks (design A, c.md D174): the phases of
//! blaze-build-script `c_tasks::run_crate` (the reference driver, run in order) with the misses
//! compiled in parallel, each by a [`ShelfClang`] of its own worker under the Seatbelt grant,
//! with a deadline per compile ([`CTasksSetting::deadline`], rule 18).
//!
//! 1. Plan the demanded archives' objects ([`c_tasks::plan`]), and the script's ASSUMPTIONS:
//!    every compile request no archive request names (`ar` never takes its object). A deferred
//!    compile is answered "exit 0" at the script's run (spawn_route.rs `Route::Deferred`), so a
//!    script that branches on one (aws-lc-sys's `compiler_check`: `c11`, `stdalign_check`,
//!    `builtin_swap_check`, `neon_sha3_check`, `apple_core_services_check`) took "it compiles"
//!    as given. `run` and `test` discharge each (a check happens once and leaves a proof, rule
//!    74): compiled or found by key like any object, a failure a typed error naming the script
//!    and the check ([`TasksError::Assumed`]); `check` alone runs none of them.
//!
//!    An assumed FAILURE cannot be said today: a check EXPECTED to fail on this target is
//!    answered success, the script takes the wrong branch, and the discharge reports that as an
//!    error (correct, not helpful). The end state is a pinned answer per (clang, target, source
//!    digest) for compile-only checks, as pins/answers/clang.json holds for flag checks; the
//!    harness can produce them. That comes after the object runner works on ring.
//! 2. Look each unit up by key ([`c_tasks::look`]) against today's facts: a path under the
//!    script's `OUT_DIR` is the `OUT_DIR` STATE the request saw at its spawn, from the record
//!    alone ([`c_tasks::StateToday`]; outputs.rs, THE C INPUTS), any other its laid root's. A
//!    request runs LATER what the script asked NOW: the script may have removed (aws-lc-sys's
//!    `remove_dir_all` of its check folder) or rewritten (cc-rs's one `flag_check.c`) what it
//!    read, and the state holds it as it was.
//! 3. Lay each state a miss saw, from the record (`<work>/states/<record digest>/<state>`), and
//!    compile the misses on `min(workers, misses)` threads, each against its state's folder as
//!    the guest `OUT_DIR`. Each thread has its own world, frame, facts and task folder
//!    (`<work>/task-<n>`); a unit crosses as its request's text and is read again there; a
//!    result crosses back as bytes in the shared heap. The first failure stops new compiles;
//!    the ones running finish.
//! 4. Record each compile in plan order ([`c_tasks::admit`]) on the calling thread (the store is
//!    the caller's, one writer).
//! 5. Make or find each archive ([`c_tasks::archive`]) in the home's file area.
//!
//! A failed compile is reported with its command line as recorded (guest paths: the script's
//! argv, `\0` shown as a space) and clang's first [`c_tasks::ERROR_LINES`] lines of standard
//! error, the laid host folders shown as their guest spellings ([`ShelfClang`]).
//!
//! 0. Before all of it, a crate's process-tree requests (tikv-jemalloc-sys's `sh configure`,
//!    `make`, `make install`) run as ONE task (c_tasks_tree.rs `run_tree`): found by key, or run
//!    once by [`ShelfTree`] in the Seatbelt grant against `OUT_DIR` laid from the state they saw,
//!    in a deterministic environment, under one deadline ([`TREE_DEADLINE`]) with capped output
//!    and the process group killed past either; its files are stored files of the built record.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use std::path::Path;

use blaze_build_script::c_tasks::{self, CCompiler, CStore, Compiled, FileRef, GuestRoots, Looked, Made, PlanError, Refused, Request, TaskError, Toolchain};
use blaze_build_script::c_tasks_built::Report;
use blaze_build_script::c_tasks_tree::{self, FIXED_ENV, TREE_LINES, TreeError, TreeExit, TreeJob, TreeRan, TreeRunner};
use blaze_build_script::host_tools::{self, HostTool};
use blaze_build_script::key::HostToolKey;
use blaze_build_script::outputs::{FileLimits, Outputs, OutputsRef, StoredFile};
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Heap, SharedAlloc, String, Vec};
use blaze_roots::{Digest, Mount, Root, RootId, RootKind, Roots};
use bstd::procs::Exit;
use bstd::procs::hermetic::{Capture, ClockSpec, Fault, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec, HermeticStreams, Level, Program, ReportSpec, Stop};

use crate::native_c::{DiskFacts, FileArea, GuestToday, Laid, ShelfClang};
use crate::native_script::{NATIVE_BUDGET, OutDir, listed_under, store_out_dir};
use crate::record::native_world;

/// Each compile's deadline: the slowest of aws-lc's objects is far under it.
pub const COMPILE_DEADLINE: Duration = Duration::from_secs(300);

/// What every crate's C tasks of one command run with.
#[derive(Clone, Copy)]
pub struct CTasksSetting<'s> {
    /// `<shelf>/bin/clang-23`, canonical.
    pub clang: &'s str,
    /// The shelf and the SDK, laid (their digests are the toolchain's identity).
    pub shelf: Laid<'s>,
    pub sdk: Laid<'s>,
    /// A folder of the home's, canonical and existing: each worker's task folder is under it.
    pub work: &'s str,
    /// The home's file area (scripts.rs `file_area`), canonical, on `work`'s volume.
    pub area: &'s str,
    /// The most compiles at once (the command's workers).
    pub workers: u32,
    /// Each compile's deadline.
    pub deadline: Duration,
    /// The heap worker results cross threads in.
    pub heap: &'static Heap,
}

/// One crate's tasks done: its archives (by name, in plan order) and what it came to.
#[derive(Debug)]
pub struct TasksRun<'a, 'b> {
    pub made: AVec<'a, Made<'b>>,
    /// The files the crate's process tree made or changed under `OUT_DIR`, sorted by name
    /// (c_tasks_tree.rs `run_tree`): stored files of the built record.
    pub tree: AVec<'a, StoredFile<'a>>,
    pub report: Report,
}

/// Why one crate's C tasks gave no archives; each names the crate (rule 19).
#[derive(Debug)]
pub enum TasksError<'a, 'b> {
    /// From the sans-IO core: the plan, a missing source, a fact, the store.
    Task(TaskError<'a, 'b>),
    /// clang refused a demanded object.
    Compile { package: &'b str, source: &'b str, command: AString<'a>, status: i32, first_lines: AString<'a>, failed: u32 },
    /// clang refused a compile the script assumed succeeds.
    Assumed { package: &'b str, source: &'b str, command: AString<'a>, status: i32, first_lines: AString<'a> },
    /// An `OUT_DIR` state could not be laid for its compiles (the record, a name, the disk).
    Lay { package: &'b str, state: u32, words: AString<'a> },
    /// The compile could not be run (the sandbox, the deadline, the file area).
    Refused { package: &'b str, source: &'b str, command: AString<'a>, words: AString<'a> },
    /// A worker could not start (its world, frame or task folder).
    Worker { package: &'b str, words: AString<'a> },
    /// The process tree (`sh configure`, `make`) gave no files (c_tasks_tree.rs).
    Tree(TreeError<'a, 'b>),
}

impl fmt::Display for TasksError<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TasksError::Task(error) => describe(error, f),
            TasksError::Compile { package, source, command, status, first_lines, failed } => {
                write!(f, "the C of {package}: `{source}` does not compile (exit {status}; {failed} compiles failed): {}\n{}", command.as_str(), first_lines.as_str())
            }
            TasksError::Assumed { package, source, command, status, first_lines } => write!(
                f,
                "the build script of {package} assumed `{source}` compiles; it does not (exit {status}): {}\n{}",
                command.as_str(),
                first_lines.as_str()
            ),
            TasksError::Lay { package, state, words } => write!(f, "the C of {package}: OUT_DIR as its requests saw it (state {state}) could not be laid: {}", words.as_str()),
            TasksError::Refused { package, source, command, words } => write!(f, "the C of {package}: `{source}` could not be compiled: {}: {}", words.as_str(), command.as_str()),
            TasksError::Worker { package, words } => write!(f, "the C of {package}: a compile worker could not start: {}", words.as_str()),
            TasksError::Tree(error) => write!(f, "{error}"),
        }
    }
}

/// The core's error in words, naming the crate.
fn describe(error: &TaskError<'_, '_>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        TaskError::Plan { package, why } => match why {
            PlanError::TooMany { requests } => write!(f, "the C of {package}: {requests} requests, more than {}", c_tasks::MOST_REQUESTS),
            PlanError::Request { error, text } => write!(f, "the C of {package}: {}", c_tasks::RefusedRequest { error: *error, text: *text }),
            PlanError::MemberNotCompiled { archive, member } => write!(f, "the C of {package}: `{archive}` takes `{member}`, which no compile writes"),
            PlanError::TwoWriters { member } => write!(f, "the C of {package}: two compiles write `{member}`"),
        },
        TaskError::NoSource { package, source } => write!(f, "the C of {package}: `{source}` is not there"),
        TaskError::Compile { package, source, status, first_lines } => write!(f, "the C of {package}: `{source}` does not compile (exit {status})\n{}", first_lines.as_str()),
        TaskError::Assumed { package, source, status, first_lines } => {
            write!(f, "the build script of {package} assumed `{source}` compiles; it does not (exit {status})\n{}", first_lines.as_str())
        }
        TaskError::Refused { package, source, words } => write!(f, "the C of {package}: `{source}` could not be compiled: {}", words.as_str()),
        TaskError::ReadOutside { package, source, path } => write!(f, "the C of {package}: compiling `{source}` read `{}`, under no root it was given", path.as_str()),
        TaskError::PathSets { package, source } => write!(f, "the C of {package}: `{source}` has read {} different sets of headers (c_tasks::PATH_SETS)", c_tasks::PATH_SETS),
        TaskError::Fact { package, path, error } => write!(f, "the C of {package}: today's fact of `{path}` could not be had (error {})", error.code),
        TaskError::Store { package, what } => write!(f, "the C of {package}: the store refused {what}"),
        TaskError::Damaged { package, key } => write!(f, "the C of {package}: the record at {key:?} is not of its form"),
    }
}

/// A worker's answer, as bytes in the shared heap.
type Bytes = Vec<u8, SharedAlloc<'static>>;

const MADE: u8 = 0;
const FAILED: u8 = 1;
const REFUSED: u8 = 2;

/// One compile to run (a miss, in plan order): its request's index, the `OUT_DIR` state it
/// saw, and its text.
#[derive(Clone, Copy)]
struct Job<'b> {
    request: u32,
    state: u32,
    text: &'b str,
}

/// One crate's tasks: `sources` are the laid package, the script's `OUT_DIR` as it ended and
/// each dependency's `OUT_DIR` a request names; `demand` the archives asked for. The script's
/// own `OUT_DIR` entry is replaced, per compile, by its state's laid folder.
#[expect(clippy::too_many_arguments, reason = "each is one input of the crate's tasks, named at the call")]
pub fn run_tasks<'a, 'b>(
    setting: &CTasksSetting<'_>,
    package: &'b str,
    outputs: &OutputsRef<'b>,
    sources: &[Laid<'_>],
    roots: GuestRoots<'_>,
    demand: &[&str],
    toolchain: &Toolchain<'_>,
    trees: &TreeTools<'_>,
    store: &mut dyn CStore,
    alloc: Alloc<'a>,
) -> Result<TasksRun<'a, 'b>, TasksError<'a, 'b>> {
    let no_worker = |words: &str| TasksError::Worker { package, words: String::from_str_in(words, alloc) };
    let (world, _) = native_world(Path::new(setting.work)).map_err(|e| no_worker(&e.to_string()))?;
    let clock = world.need_clock().map_err(|missing| no_worker(missing.capability))?;
    let started = clock.now().since_start();

    let plan = c_tasks::plan(outputs, &roots, demand, alloc).map_err(|why| TasksError::Task(TaskError::Plan { package, why }))?;
    let c_tasks::Plan { units, planned, archives, counts, trees: tree_requests } = plan;

    // The process tree first (c_tasks_tree.rs): what it makes is under `OUT_DIR`, found by its
    // key or run once, sandboxed, by [`ShelfTree`].
    let tree = match tree_requests.is_empty() {
        true => Vec::new_in(alloc),
        false => {
            let mut keyed: AVec<'a, (&str, [u8; 32])> = Vec::with_capacity_in(sources.len(), alloc);
            keyed.extend(sources.iter().filter(|laid| laid.guest != roots.out).map(|laid| (laid.guest, laid.digest)));
            let inputs = c_tasks_tree::TreeInputs { toolchain, tools: trees.keys, os_build: trees.os_build, sources: &keyed, runner: TREE_RUNNER };
            let mut runner = ShelfTree { setting, sources, roots, tools: trees, demand };
            let done = c_tasks_tree::run_tree(package, outputs, &roots, &tree_requests, &inputs, &mut runner, store, alloc).map_err(TasksError::Tree)?;
            done.files
        }
    };
    let planned = planned as usize;
    let mut texts: Vec<&'b str, SharedAlloc<'static>> = Vec::new_in(setting.heap.shared());
    texts.extend(outputs.c_requests());

    // Look each unit up against today's facts: its state's for `OUT_DIR`, the laid roots' else.
    let facts = DiskFacts::new(alloc);
    let today = GuestToday { laid: sources, facts: &facts };
    let state_today = |state: u32| c_tasks::StateToday { outputs, state, rest: &today };
    let mut looked: AVec<'a, Looked> = Vec::with_capacity_in(units.len(), alloc);
    for unit in units.iter() {
        looked.push(c_tasks::look(package, unit, toolchain, &state_today(unit.state), store, alloc).map_err(TasksError::Task)?);
    }
    let mut jobs: Vec<Job<'b>, SharedAlloc<'static>> = Vec::new_in(setting.heap.shared());
    for (at, one) in looked.iter().enumerate() {
        if let Looked::Miss { .. } = one {
            let (request, state) = (units[at].request, units[at].state);
            jobs.push(Job { request, state, text: texts[request as usize] });
        }
    }

    // Lay each state a miss saw; each state's roots are `sources` with the script's `OUT_DIR`
    // at its state's folder.
    let states = outputs.c_state_count() as usize;
    let mut wanted: AVec<'a, bool> = Vec::new_in(alloc);
    wanted.resize(states, false);
    for job in jobs.iter() {
        wanted[job.state as usize] = true;
    }
    let record = hex(&outputs.crate_digest().0, alloc);
    let mut folders: AVec<'a, AString<'a>> = Vec::with_capacity_in(states, alloc);
    for (state, &wanted) in wanted.iter().enumerate() {
        let mut folder = String::new_in(alloc);
        if wanted {
            folder.push_str(setting.work);
            folder.push_str("/states/");
            folder.push_str(record.as_str());
            folder.push('/');
            folder.push_str(itoa(state as u32, &mut [0u8; 10]));
            lay_state(outputs, state as u32, folder.as_str(), alloc).map_err(|words| TasksError::Lay { package, state: state as u32, words })?;
        }
        folders.push(folder);
    }
    let mut state_roots: AVec<'a, AVec<'a, Laid<'_>>> = Vec::with_capacity_in(states, alloc);
    for (state, folder) in folders.iter().enumerate() {
        let mut laid: AVec<'a, Laid<'_>> = Vec::with_capacity_in(sources.len() + 1, alloc);
        if wanted[state] {
            let mut digest = blaze_ids::Hasher::new("c_tasks laid state");
            digest.update(&outputs.crate_digest().0).update(&(state as u32).to_le_bytes());
            let seen = Laid { guest: roots.out, host: folder.as_str(), digest: digest.finish().0, name: "OUT_DIR as the request saw it" };
            // In place, so each root keeps its index.
            laid.extend(sources.iter().map(|root| if root.guest == roots.out { seen } else { *root }));
            if !sources.iter().any(|root| root.guest == roots.out) {
                laid.push(seen);
            }
        }
        state_roots.push(laid);
    }
    let mut by_state: Vec<&[Laid<'_>], SharedAlloc<'static>> = Vec::new_in(setting.heap.shared());
    by_state.extend(state_roots.iter().map(|laid| laid.as_slice()));

    // Compile the misses in parallel.
    let answers = compile_all(setting, &by_state, roots, &jobs).map_err(|words| no_worker(&words))?;

    // Record them in plan order; the first failure, in plan order, is the error.
    let mut report = Report { requests: counts.requests, planned: planned as u32, assumed: counts.assumed, archives: archives.len() as u32, ..Report::default() };
    let mut objects: AVec<'a, FileRef> = Vec::with_capacity_in(units.len(), alloc);
    let mut first: Option<TasksError<'a, 'b>> = None;
    let mut answer_at = 0usize;
    for (at, unit) in units.iter().enumerate() {
        let (pre, free_slot) = match looked[at] {
            Looked::Hit(file) => {
                report.found += 1;
                objects.push(file);
                continue;
            }
            Looked::Miss { pre, free_slot } => (pre, free_slot),
        };
        let answer = answers.get(answer_at).and_then(|it| it.as_ref());
        answer_at += 1;
        let command = || shown(texts[unit.request as usize], alloc);
        let Some(answer) = answer else {
            // Not run: a failure stopped new compiles first.
            objects.push(FileRef { sha256: [0; 32], len: 0 });
            continue;
        };
        match decode(answer, alloc) {
            Answer::Made { object, reads } => {
                c_tasks::admit(package, unit, pre, free_slot, object, &reads, &roots, &state_today(unit.state), store, alloc).map_err(TasksError::Task)?;
                report.compiled += 1;
                objects.push(object);
            }
            Answer::Failed { status, first_lines } => {
                report.failed += 1;
                objects.push(FileRef { sha256: [0; 32], len: 0 });
                if first.is_none() {
                    first = Some(match at >= planned {
                        true => TasksError::Assumed { package, source: unit.source, command: command(), status, first_lines },
                        false => TasksError::Compile { package, source: unit.source, command: command(), status, first_lines, failed: 0 },
                    });
                }
            }
            Answer::Refused { words } => {
                report.failed += 1;
                objects.push(FileRef { sha256: [0; 32], len: 0 });
                if first.is_none() {
                    first = Some(TasksError::Refused { package, source: unit.source, command: command(), words });
                }
            }
        }
    }
    if let Some(mut error) = first {
        if let TasksError::Compile { failed, .. } = &mut error {
            *failed = report.failed;
        }
        return Err(error);
    }

    // The archives, from the demanded objects alone.
    let mut area = FileArea { folder: setting.area };
    let mut made = Vec::with_capacity_in(archives.len(), alloc);
    for plan in archives.iter() {
        let (one, ran) = c_tasks::archive(package, plan, &units[..planned], &objects[..planned], &mut area, store, alloc).map_err(TasksError::Task)?;
        report.archives_made += u32::from(ran);
        report.archive_bytes += one.file.len;
        made.push(one);
    }
    report.ms = clock.now().since_start().saturating_sub(started).as_millis() as u64;
    Ok(TasksRun { made, tree, report })
}

/// Lays the `OUT_DIR` state `state` of `outputs` at `folder` (made anew: a folder left by an
/// earlier command is removed first, so nothing but the state is there). A name that is not a
/// plain relative path (`..`, a leading `/`, an empty part) is refused: it would escape the
/// folder.
fn lay_state<'a>(outputs: &OutputsRef<'_>, state: u32, folder: &str, alloc: Alloc<'a>) -> Result<(), AString<'a>> {
    let words = |text: &str| String::from_str_in(text, alloc);
    let entries = c_tasks::laid_state(outputs, state, alloc).map_err(|why| words(&why.to_string()))?;
    match std::fs::remove_dir_all(folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(words(&format!("{folder}: {error}"))),
    }
    std::fs::create_dir_all(folder).map_err(|e| words(&format!("{folder}: {e}")))?;
    for (name, bytes) in entries.iter() {
        if name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(words(&format!("the state names `{name}`, which is not a path under OUT_DIR")));
        }
        let path = format!("{folder}/{name}");
        let done = match bytes {
            None => std::fs::create_dir_all(&path),
            Some(bytes) => std::fs::write(&path, bytes),
        };
        done.map_err(|e| words(&format!("{path}: {e}")))?;
    }
    Ok(())
}

/// `bytes` in lower-case hex.
fn hex<'a>(bytes: &[u8], alloc: Alloc<'a>) -> AString<'a> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity_in(bytes.len() * 2, alloc);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// `n` in decimal, written into `buf`.
fn itoa(mut n: u32, buf: &mut [u8; 10]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[at..]).expect("native_c_tasks itoa: digits are ASCII")
}

/// A request's text as a command line (its argv, `\0` a space): guest paths, as the script ran.
fn shown<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for (at, word) in text.split('\0').enumerate() {
        if at > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A worker's answer, read back.
enum Answer<'a> {
    Made { object: FileRef, reads: AVec<'a, AString<'a>> },
    Failed { status: i32, first_lines: AString<'a> },
    Refused { words: AString<'a> },
}

fn encode_made(object: FileRef, reads: &[AString<'_>], out: &mut Bytes) {
    out.push(MADE);
    out.extend_from_slice(&object.sha256);
    out.extend_from_slice(&object.len.to_le_bytes());
    for read in reads {
        out.extend_from_slice(read.as_str().as_bytes());
        out.push(0);
    }
}

fn encode_text(tag: u8, status: i32, text: &str, out: &mut Bytes) {
    out.push(tag);
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// `bytes` as [`encode_made`] or [`encode_text`] wrote them; a malformed answer (no worker
/// writes one) is a refusal, which the caller names by the unit's source.
fn decode<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Answer<'a> {
    let text = |bytes: &[u8]| String::from_str_in(core::str::from_utf8(bytes).unwrap_or("(not UTF-8)"), alloc);
    let malformed = || Answer::Refused { words: String::from_str_in("a compile worker's answer is malformed (native_c_tasks.rs `decode`)", alloc) };
    match bytes.split_first() {
        Some((&MADE, rest)) if rest.len() >= 40 => {
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&rest[..32]);
            let mut len = [0u8; 8];
            len.copy_from_slice(&rest[32..40]);
            let mut reads = Vec::new_in(alloc);
            for read in rest[40..].split(|b| *b == 0) {
                if !read.is_empty() {
                    reads.push(text(read));
                }
            }
            Answer::Made { object: FileRef { sha256, len: u64::from_le_bytes(len) }, reads }
        }
        Some((&tag, rest)) if (tag == FAILED || tag == REFUSED) && rest.len() >= 4 => {
            let mut status = [0u8; 4];
            status.copy_from_slice(&rest[..4]);
            match tag {
                FAILED => Answer::Failed { status: i32::from_le_bytes(status), first_lines: text(&rest[4..]) },
                _ => Answer::Refused { words: text(&rest[4..]) },
            }
        }
        _ => malformed(),
    }
}

/// Each job's answer, in job order (`None`: not run, a failure stopped new compiles first);
/// `Err` when a worker could not start.
fn compile_all<'b>(
    setting: &CTasksSetting<'_>,
    sources: &[&[Laid<'_>]],
    roots: GuestRoots<'_>,
    jobs: &[Job<'b>],
) -> Result<Vec<Option<Bytes>, SharedAlloc<'static>>, std::string::String> {
    let shared = setting.heap.shared();
    let mut answers: Vec<Option<Bytes>, SharedAlloc<'static>> = Vec::new_in(shared);
    answers.resize_with(jobs.len(), || None);
    if jobs.is_empty() {
        return Ok(answers);
    }
    let workers = (setting.workers.max(1) as usize).min(jobs.len());
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let done = std::thread::scope(|scope| {
        let mut handles = std::vec::Vec::with_capacity(workers);
        for n in 0..workers {
            let (next, stop) = (&next, &stop);
            handles.push(scope.spawn(move || worker(setting, sources, roots, jobs, n, next, stop)));
        }
        let mut all = std::vec::Vec::with_capacity(workers);
        for handle in handles {
            all.push(handle.join().map_err(|_| "a compile worker panicked".to_owned()).and_then(|it| it));
        }
        all
    });
    for one in done {
        for (job, answer) in one? {
            answers[job as usize] = Some(answer);
        }
    }
    Ok(answers)
}

/// One worker: its own world, frame, facts and task folder; jobs taken in turn until none is
/// left or `stop`. A job compiles against `sources[job.state]`, its state's roots.
fn worker<'b>(
    setting: &CTasksSetting<'_>,
    sources: &[&[Laid<'_>]],
    roots: GuestRoots<'_>,
    jobs: &[Job<'b>],
    n: usize,
    next: &AtomicUsize,
    stop: &AtomicBool,
) -> Result<Vec<(u32, Bytes), SharedAlloc<'static>>, std::string::String> {
    let shared = setting.heap.shared();
    let mut answers: Vec<(u32, Bytes), SharedAlloc<'static>> = Vec::new_in(shared);
    let task_out = format!("{}/task-{n}", setting.work);
    std::fs::create_dir_all(&task_out).map_err(|e| format!("{task_out}: {e}"))?;
    let (world, _) = native_world(Path::new(setting.work)).map_err(|e| e.to_string())?;
    let spawner = world.need_host().and_then(|host| host.spawner()).map_err(|missing| missing.capability.to_owned())?;
    let clock = world.need_clock().map_err(|missing| missing.capability.to_owned())?;
    let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: world.need_clock().map_err(|missing| missing.capability.to_owned())? });
    let cx = world.cx(Duration::MAX, bstd::mem::TASK_FRAME).map_err(|_| "the worker's world stopped".to_owned())?;
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Other).map_err(|e| format!("no frame for a compile worker: {e:?}"))?;
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let facts = DiskFacts::new(alloc);
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let at = next.fetch_add(1, Ordering::AcqRel);
        let Some(job) = jobs.get(at) else { break };
        let mut clang = ShelfClang {
            hermetic: &hermetic,
            cx: &cx,
            clock: &clock,
            clang: setting.clang,
            shelf: setting.shelf,
            sdk: setting.sdk,
            sources: sources[job.state as usize],
            roots,
            task_out: &task_out,
            area: setting.area,
            facts: &facts,
            deadline: setting.deadline,
        };
        let mut bytes: Bytes = Vec::new_in(shared);
        match c_tasks::read(job.request, job.state, job.text, &roots, alloc) {
            Ok(Request::Object(unit)) => match clang.compile(&unit, alloc) {
                Ok(Compiled::Made { object, reads }) => encode_made(object, &reads, &mut bytes),
                Ok(Compiled::Failed { status, first_lines }) => {
                    stop.store(true, Ordering::Release);
                    encode_text(FAILED, status, first_lines.as_str(), &mut bytes);
                }
                Err(refused) => {
                    stop.store(true, Ordering::Release);
                    encode_text(REFUSED, 0, refused.words.as_str(), &mut bytes);
                }
            },
            _ => {
                stop.store(true, Ordering::Release);
                encode_text(REFUSED, 0, "the request is no object compile when read again (native_c_tasks.rs `worker`)", &mut bytes);
            }
        }
        answers.push((at as u32, bytes));
    }
    Ok(answers)
}

// ---------------------------------------------------------------------------------------------
// The process tree

/// A tree's deadline, all its steps together (rule 18): jemalloc's `configure` and `make` on
/// one job are a few minutes.
pub const TREE_DEADLINE: Duration = Duration::from_secs(20 * 60);

/// Standard output and error kept of one step; past it the step is stopped (rule 18).
const TREE_STREAM: u32 = 4 << 20;

/// Every laid file's modification time (2000-01-01): `make` compares times, and a laid tree's
/// must be the same on every machine and older than anything a step writes.
const LAID_MTIME: u64 = 946_684_800;

/// The runner's form, in the tree's key (c_tasks_tree.rs `TreeInputs::runner`): what
/// [`ShelfTree`] adds to the environment and how it lays the state. A change to either is a
/// new form.
pub const TREE_RUNNER: &str = "native_c_tasks ShelfTree 1";

/// The host tools one crate's tree may start (pins/clang.json `host_tools`, `granted` to its
/// package), each by its SHA-256 (native_runner_c.rs), and the OS build their libraries and
/// shells are of.
#[derive(Clone, Copy)]
pub struct TreeTools<'t> {
    pub tools: &'t [HostTool<'t>],
    pub keys: &'t [HostToolKey<'t>],
    pub os_build: &'t str,
}

/// `package`'s [`TreeTools`] on this machine: the tools the table grants it (pins/clang.json
/// `host_tools` for `host`), each program at its real path keyed by its SHA-256 (a tool under
/// its name, each program it starts under `<tool>+<path>`, as native_unit.rs keys a native
/// script's), and the OS build (`/System/Library/CoreServices/SystemVersion.plist`, fact_pins.rs
/// `read_apple_userland`, the one place it is read). Held in `alloc` for the command. A program
/// whose path is not its real path is refused: the grant names real paths (Seatbelt matches
/// them), and a link's target could change under an equal key.
pub fn tree_tools<'a>(package: &str, host: &'static str, shelf: &str, bump: blaze_own::Bump<'a>) -> Result<TreeTools<'a>, AString<'a>> {
    let alloc = bump.alloc();
    let words = |text: std::string::String| String::from_str_in(&text, alloc);
    let table = host_tools::host_tools(blaze_pins::CLANG_JSON, host, alloc).map_err(|e| words(format!("the host tools' table: {e}")))?;
    let tools: AVec<'a, HostTool<'static>> = host_tools::granted(&table, package, alloc);
    let mut keys: blaze_own::BVec<'a, HostToolKey<'a>> = Vec::new_in(bump);
    for tool in tools.iter() {
        for (at, program) in tool.programs_in(shelf, alloc).into_iter().enumerate() {
            let program = program.as_str();
            let real = std::fs::canonicalize(program).map_err(|e| words(format!("{program}: {e}")))?;
            if real.as_os_str() != program {
                return Err(words(format!("the host tool `{}` is granted at {program}, whose real path is {}: the table names real paths", tool.name, real.display())));
            }
            let bytes = std::fs::read(program).map_err(|e| words(format!("{program}: {e}")))?;
            let name = match at {
                0 => bump.alloc_str(tool.name),
                _ => bump.alloc_str(&format!("{}+{program}", tool.name)),
            };
            keys.push(HostToolKey::of(tool, name, blaze_sha256::digest(&bytes)));
        }
    }
    let text = crate::fact_pins::read_apple_userland(alloc).map_err(|e| words(format!("the OS build: {e}")))?;
    let userland = blaze_build_script::fact_pins::apple_userland(text.plist.as_str(), text.darwin_release.as_str()).map_err(|e| words(format!("the OS build: {e}")))?;
    let os_build = bump.alloc_str(userland.build_version);
    let tools_slice = bump.alloc_slice_copy(&tools);
    Ok(TreeTools { tools: tools_slice, keys: keys.into_bump_slice(), os_build })
}

/// The digest of what a crate's tree is run with beyond its record (its tools, the OS build,
/// the runner's form): the built record's key holds it (c_tasks_built.rs `built_key`), so a
/// changed tool or OS build is another built record, never the old one found (rule 21).
pub fn tree_tools_digest(trees: &TreeTools<'_>) -> [u8; 32] {
    let mut hasher = blaze_ids::Hasher::new("native_c_tasks tree tools 1");
    hasher.part(TREE_RUNNER.as_bytes()).part(trees.os_build.as_bytes());
    hasher.update(&(trees.keys.len() as u64).to_le_bytes());
    for key in trees.keys {
        hasher.part(key.name.as_bytes()).part(&key.digest).update(&[u8::from(key.runs_out_dir)]);
    }
    hasher.finish().0
}

/// The tree strategy of the command (c_tasks_tree.rs [`TreeRunner`]): each step a hermetic
/// run at `Level::Inputs` under the Seatbelt grant (rule 9), against `OUT_DIR` laid from the
/// state the steps saw at `<work>/trees/<key>/out`; its environment the recorded one with each
/// guest folder made its host folder, the C toolchain the shelf's, and [`FIXED_ENV`].
struct ShelfTree<'s> {
    setting: &'s CTasksSetting<'s>,
    /// The laid roots (the script's own `OUT_DIR` among them is replaced by the tree's).
    sources: &'s [Laid<'s>],
    roots: GuestRoots<'s>,
    tools: &'s TreeTools<'s>,
    /// What the crate's link lines ask for, relative to `OUT_DIR`: a demanded file the tree
    /// left with a host folder in it is refused (rule 25), never left out.
    demand: &'s [&'s str],
}

/// A byte that ends a word of a command line or a variable's value: a guest folder begins
/// only after one (or after a flag's letters: `-I/out/include`).
fn ends_word(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'=' | b':' | b'"' | b'\'' | b',')
}

/// A byte of a file or folder name: a guest spelling followed by one is another name
/// (`/out` is not `/outer`).
fn names_on(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

/// `text` with each guest folder of `pairs` (guest, host; longest guest first) made its host
/// folder where a word begins with it, or a flag's letters do (`-I/out/x`, `--prefix=/out`).
/// A guest spelling inside a word (`a/out`) or followed by a name's byte is left as it is.
fn to_host<'a>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'a>) -> AString<'a> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity_in(text.len(), alloc);
    let (mut word, mut slashed, mut copied, mut at) = (0usize, false, 0usize, 0usize);
    while at < bytes.len() {
        let byte = bytes[at];
        if ends_word(byte) {
            (word, slashed) = (at + 1, false);
            at += 1;
            continue;
        }
        if byte == b'/' && !slashed {
            slashed = true;
            let before = &text[word..at];
            if before.is_empty() || before.starts_with('-') {
                let rest = &text[at..];
                let found = pairs.iter().find(|(guest, _)| rest.starts_with(guest) && !bytes.get(at + guest.len()).is_some_and(|next| names_on(*next)));
                if let Some((guest, host)) = found {
                    out.push_str(&text[copied..at]);
                    out.push_str(host);
                    at += guest.len();
                    copied = at;
                    continue;
                }
            }
        }
        at += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// `text` with each host folder of `pairs` (guest, host) shown as its guest spelling, the
/// longest host first: what an error shows (rule 19) and a file is checked against.
fn to_guest<'a>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'a>) -> AString<'a> {
    let mut by_host: AVec<'a, (&str, &str)> = Vec::with_capacity_in(pairs.len(), alloc);
    by_host.extend_from_slice(pairs);
    by_host.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    let mut shown = String::from_str_in(text, alloc);
    for (guest, host) in by_host.iter() {
        shown = replace_all(shown.as_str(), host, guest, alloc);
    }
    shown
}

fn replace_all<'a>(text: &str, from: &str, to: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let mut rest = text;
    while let Some(at) = rest.find(from) {
        out.push_str(&rest[..at]);
        out.push_str(to);
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    out
}

/// The shelf program a C toolchain variable's first word names, by its file name: the tree's
/// compiler and archiver are always the pinned ones (`cc`, `gcc` and `clang` are the shelf's
/// `clang`; `c++`, `g++`, `clang++` its `clang++`; `ar` its `llvm-ar`); `None` leaves the word.
fn shelf_program(word: &str) -> Option<&'static str> {
    let name = word.rsplit('/').next().unwrap_or(word);
    match name {
        "cc" | "gcc" | "clang" | "clang-23" => Some("bin/clang"),
        "c++" | "g++" | "clang++" => Some("bin/clang++"),
        "ar" | "llvm-ar" => Some("bin/llvm-ar"),
        _ => None,
    }
}

/// A variable's value made one string from parts.
fn joined<'a>(parts: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for part in parts {
        out.push_str(part);
    }
    out
}

fn refused_words<'a>(parts: &[&str], alloc: Alloc<'a>) -> Refused<'a> {
    Refused { words: joined(parts, alloc) }
}

/// Whether `bytes` holds `needle` anywhere.
fn holds(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle)
}

/// Lays `entries` (an `OUT_DIR` state: c_tasks.rs `laid_state`) at `folder`, made anew: each
/// file at [`LAID_MTIME`], one that begins `#!` executable (`make` starts a tree's own
/// `install-sh` by its path).
fn lay_tree<'a>(entries: &[(&str, Option<&[u8]>)], folder: &str, alloc: Alloc<'a>) -> Result<(), AString<'a>> {
    use std::os::unix::fs::PermissionsExt as _;
    let words = |text: &str| String::from_str_in(text, alloc);
    let laid_at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(LAID_MTIME);
    for (name, bytes) in entries {
        if name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(words(&format!("the state names `{name}`, which is not a path under OUT_DIR")));
        }
        let path = format!("{folder}/{name}");
        let failed = |e: std::io::Error| words(&format!("{path}: {e}"));
        match bytes {
            None => std::fs::create_dir_all(&path).map_err(failed)?,
            Some(bytes) => {
                if let Some((parent, _)) = path.rsplit_once('/') {
                    std::fs::create_dir_all(parent).map_err(failed)?;
                }
                std::fs::write(&path, bytes).map_err(failed)?;
                let file = std::fs::File::options().write(true).open(&path).map_err(failed)?;
                file.set_modified(laid_at).map_err(failed)?;
                let mode = if bytes.starts_with(b"#!") { 0o755 } else { 0o644 };
                file.set_permissions(std::fs::Permissions::from_mode(mode)).map_err(failed)?;
            }
        }
    }
    Ok(())
}

impl ShelfTree<'_> {
    /// The folders a step reads, each a tree root by what names it: the laid roots, the SDK,
    /// the shelf, each granted tool's runtime folders (by the tool's digest).
    fn read_roots<'a>(&self, alloc: Alloc<'a>) -> AVec<'a, (&str, [u8; 32])> {
        let mut out: AVec<'a, (&str, [u8; 32])> = Vec::new_in(alloc);
        for laid in self.sources.iter().filter(|laid| laid.guest != self.roots.out) {
            out.push((laid.host, laid.digest));
        }
        out.push((self.setting.sdk.host, self.setting.sdk.digest));
        out.push((self.setting.shelf.host, self.setting.shelf.digest));
        for tool in self.tools.tools {
            let digest = self.tools.keys.iter().find(|key| key.name == tool.name).map_or([0; 32], |key| key.digest);
            for folder in tool.read_folders() {
                if !out.iter().any(|(seen, _)| *seen == folder) {
                    out.push((folder, digest));
                }
            }
        }
        out
    }
}

impl TreeRunner for ShelfTree<'_> {
    fn run<'a>(&mut self, job: &TreeJob<'_, '_>, outputs: &OutputsRef<'_>, key: ContentId, alloc: Alloc<'a>) -> Result<TreeRan<'a>, Refused<'a>> {
        let setting = self.setting;
        let (shelf, sdk) = (setting.shelf.host, setting.sdk.host);
        let fail = |parts: &[&str]| refused_words(parts, alloc);

        // 1. The tree's folder made anew, the state laid as its `out`, its own `tmp`.
        let named = hex(&key.0, alloc);
        let folder = joined(&[setting.work, "/trees/", named.as_str()], alloc);
        let out = joined(&[folder.as_str(), "/out"], alloc);
        let tmp = joined(&[folder.as_str(), "/tmp"], alloc);
        match std::fs::remove_dir_all(folder.as_str()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(fail(&[folder.as_str(), ": ", &error.to_string()])),
        }
        for made in [out.as_str(), tmp.as_str()] {
            std::fs::create_dir_all(made).map_err(|e| fail(&[made, ": ", &e.to_string()]))?;
        }
        let mut laid = c_tasks::laid_state(outputs, job.state, alloc).map_err(|why| fail(&["its OUT_DIR state: ", &why.to_string()]))?;
        laid.sort_unstable_by(|a, b| a.0.cmp(b.0));
        lay_tree(&laid, out.as_str(), alloc).map_err(|words| Refused { words })?;

        // 2. Each guest folder and its host folder, the longest guest first.
        let mut pairs: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
        pairs.push((self.roots.out, out.as_str()));
        for laid in self.sources.iter().filter(|laid| laid.guest != self.roots.out) {
            pairs.push((laid.guest, laid.host));
        }
        pairs.push((setting.sdk.guest, sdk));
        pairs.push((setting.shelf.guest, shelf));
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        // What a file is checked against and an error shows: every host folder, `tmp` too.
        let mut shown = pairs.clone();
        shown.push(("/tmp", tmp.as_str()));

        // 3. The grant: the shelf's compilers and archiver, the linker when a tool runs what
        // the tree built (D112), each granted tool and what it starts; read the roots; write
        // the tree's folder alone.
        let runs_out_dir = host_tools::runs_out_dir(self.tools.tools);
        let linker: &[&str] = if runs_out_dir { &["bin/ld64.lld", "bin/ld.lld", "bin/lld"] } else { &[] };
        let mut held: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        for name in ["bin/clang-23", "bin/clang", "bin/clang++", "bin/llvm-ar"].iter().chain(linker) {
            let path = joined(&[shelf, "/", name], alloc);
            if Path::new(path.as_str()).exists() {
                held.push(path);
            }
        }
        for tool in self.tools.tools {
            for program in tool.programs_in(shelf, alloc) {
                if Path::new(program.as_str()).exists() {
                    held.push(program);
                }
            }
        }
        let mut exec: AVec<'a, &str> = Vec::with_capacity_in(held.len(), alloc);
        exec.extend(held.iter().map(|program| program.as_str()));
        let read_roots = self.read_roots(alloc);
        let out_only = [out.as_str()];
        let exec_under: &[&str] = if runs_out_dir { &out_only } else { &[] };

        // 4. The roots: each read folder a tree root by its digest, the tree's folder the
        // output root (`out` and `tmp` under it).
        let mut root_list: AVec<'a, Root<'_>> = Vec::with_capacity_in(read_roots.len() + 1, alloc);
        let mut mounts: AVec<'a, Mount<'_>> = Vec::with_capacity_in(read_roots.len() + 1, alloc);
        for (at, (folder, digest)) in read_roots.iter().enumerate() {
            let at = u8::try_from(at).map_err(|_| fail(&["more than 255 folders read by a process tree"]))?;
            root_list.push(Root { name: "a folder a process tree reads", kind: RootKind::Tree { tree: Digest(*digest) } });
            mounts.push(Mount { spelling: folder.as_bytes(), root: RootId(at) });
        }
        let output = RootId(u8::try_from(root_list.len()).map_err(|_| fail(&["more than 255 folders read by a process tree"]))?);
        root_list.push(Root { name: "a process tree's folder", kind: RootKind::Output });
        mounts.push(Mount { spelling: folder.as_bytes(), root: output });
        let files = OutDir { root: output, folder: folder.as_str(), package_root: None };

        // 5. The world, its clock and the hermetic capability: one deadline for every step.
        let (world, _) = native_world(std::path::Path::new(setting.work)).map_err(|e| fail(&["the tree's world: ", &e.to_string()]))?;
        let spawner = world.need_host().and_then(|host| host.spawner()).map_err(|missing| fail(&["the tree's world has no ", missing.capability]))?;
        let clock = world.need_clock().map_err(|missing| fail(&["the tree's world has no ", missing.capability]))?;
        let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: world.need_clock().map_err(|missing| fail(&["the tree's world has no ", missing.capability]))? });
        let cx = world.cx(TREE_DEADLINE.saturating_add(Duration::from_secs(60)), bstd::mem::TASK_FRAME).map_err(|_| fail(&["the tree's world stopped"]))?;
        let ends = clock.now().since_start().saturating_add(TREE_DEADLINE);
        let path = host_tools::path_folders(self.tools.tools, alloc);
        let mut path_value: AString<'a> = String::new_in(alloc);
        for folder in path.iter() {
            if !path_value.is_empty() {
                path_value.push(':');
            }
            path_value.push_str(folder);
        }
        let tool_env = host_tools::tool_env(self.tools.tools, shelf, alloc);

        for step in job.steps.iter() {
            let request = step.request;
            let mut argv = step.argv();
            let Some(first) = argv.next() else { return Err(fail(&["a process-tree request with no program"])) };
            // The program: a granted tool by its name, or (a tool that runs what the tree
            // built) a program under OUT_DIR by its guest path.
            let (program, digest) = match first.contains('/') {
                true => {
                    let host = to_host(first, &pairs, alloc);
                    if !runs_out_dir || !host.as_str().starts_with(out.as_str()) {
                        return Err(fail(&["`", first, "` is under no folder a process tree may start a program from"]));
                    }
                    (host, key.0)
                }
                false => {
                    let Some(tool) = self.tools.tools.iter().find(|tool| tool.name == first) else {
                        return Err(fail(&["`", first, "` is not a host tool the table grants this package (pins/clang.json `host_tools`)"]));
                    };
                    let Some(keyed) = self.tools.keys.iter().find(|keyed| keyed.name == tool.name) else {
                        return Err(fail(&["the host tool `", first, "` has no digest"]));
                    };
                    (tool.program_in(shelf, alloc), keyed.digest)
                }
            };
            let mut held_args: AVec<'a, AString<'a>> = Vec::new_in(alloc);
            held_args.extend(argv.map(|arg| to_host(arg, &pairs, alloc)));
            let mut args: AVec<'a, &str> = Vec::with_capacity_in(held_args.len(), alloc);
            args.extend(held_args.iter().map(|arg| arg.as_str()));
            let cwd = to_host(step.tree.cwd, &pairs, alloc);

            // The environment: the recorded one at host folders, then the run's own.
            let mut vars: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(alloc);
            for (name, value) in step.tree.env() {
                if matches!(name, "MAKEFLAGS" | "MFLAGS" | "CARGO_MAKEFLAGS") && value.contains("jobserver") {
                    return Err(fail(&["the process tree's `", name, "` names a jobserver, which a tree run alone cannot be given"]));
                }
                let mut value = to_host(value, &pairs, alloc);
                if matches!(name, "CC" | "CXX" | "AR") {
                    let (word, rest) = value.as_str().split_once(' ').unwrap_or((value.as_str(), ""));
                    if let Some(member) = shelf_program(word) {
                        let space = if rest.is_empty() { "" } else { " " };
                        value = joined(&[shelf, "/", member, space, rest], alloc);
                    }
                }
                vars.push((String::from_str_in(name, alloc), value));
            }
            let set = |vars: &AVec<'a, (AString<'a>, AString<'a>)>, name: &str| vars.iter().any(|(set, _)| set.as_str() == name);
            if !set(&vars, "AR") {
                vars.push((String::from_str_in("AR", alloc), joined(&[shelf, "/bin/llvm-ar"], alloc)));
            }
            if !set(&vars, "RANLIB") {
                vars.push((String::from_str_in("RANLIB", alloc), joined(&[shelf, "/bin/llvm-ar s"], alloc)));
            }
            let mut maps: AString<'a> = String::new_in(alloc);
            for (guest, host) in shown.iter() {
                maps.push_str(" -ffile-prefix-map=");
                maps.push_str(host);
                maps.push('=');
                maps.push_str(guest);
            }
            maps.push_str(" -Wno-unused-command-line-argument");
            // The maps on the flags the tree's compiles take (`CFLAGS` always, `CXXFLAGS` when
            // the script set it): no host folder reaches an object's debug information.
            for flags in ["CFLAGS", "CXXFLAGS"] {
                match vars.iter().rposition(|(name, _)| name.as_str() == flags) {
                    Some(at) => vars[at].1.push_str(maps.as_str()),
                    None if flags == "CFLAGS" => vars.push((String::from_str_in(flags, alloc), String::from_str_in(maps.as_str().trim_start(), alloc))),
                    None => {}
                }
            }
            for (name, value) in tool_env.iter() {
                vars.push((name.clone_in(alloc), value.clone_in(alloc)));
            }
            vars.push((String::from_str_in("MACOSX_DEPLOYMENT_TARGET", alloc), String::from_str_in(blaze_build_script::toolchain::MACOS_MIN, alloc)));
            vars.push((String::from_str_in("PATH", alloc), path_value.clone_in(alloc)));
            vars.push((String::from_str_in("SDKROOT", alloc), String::from_str_in(sdk, alloc)));
            vars.push((String::from_str_in("TMPDIR", alloc), tmp.clone_in(alloc)));
            for (name, value) in FIXED_ENV {
                vars.push((String::from_str_in(name, alloc), String::from_str_in(value, alloc)));
            }
            // Sorted by name, a later value of one name the one kept.
            vars.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            let mut env: AVec<'a, (&str, &str)> = Vec::with_capacity_in(vars.len(), alloc);
            for (name, value) in vars.iter() {
                match env.last_mut() {
                    Some(last) if last.0 == name.as_str() => *last = (name.as_str(), value.as_str()),
                    _ => env.push((name.as_str(), value.as_str())),
                }
            }

            let roots = Roots::new(&root_list).map_err(|_| fail(&["the roots of a process tree were refused"]))?;
            let now = clock.now().since_start();
            let left = ends.saturating_sub(now);
            if left.is_zero() {
                return Ok(TreeRan::Failed { request, exit: TreeExit::Deadline, last_lines: String::new_in(alloc) });
            }
            let name = program.as_str().rsplit('/').next().unwrap_or(program.as_str());
            let spec = HermeticSpec::new(HermeticOpts {
                program: Program::Stored { name, digest: Digest(digest), path: program.as_str() },
                args: &args,
                env: &env,
                roots,
                mounts: &mounts,
                cwd: cwd.as_str(),
                helpers: &exec,
                helper_folders: exec_under,
                channels: &[],
                clock: ClockSpec::Fixed { unix_secs: 0 },
                seed: 0,
                budget: NATIVE_BUDGET.of(left),
                deadline: Deadline::after(now, left),
                streams: HermeticStreams { stdout: Capture::Keep { most: TREE_STREAM }, stderr: Capture::Keep { most: TREE_STREAM } },
                level: Level::Inputs,
                report: ReportSpec::WholeRoots,
            })
            .map_err(|error| refused_words(&["the process tree's spec was refused: ", &format!("{:?} ({:?})", error.fault, error.rule)], alloc))?;
            let ran = match hermetic.run(&cx, &spec, &files, alloc) {
                Ok(ran) => ran,
                Err(error) => {
                    return match error.fault {
                        Fault::Stopped(Stop::Deadline) => Ok(TreeRan::Failed { request, exit: TreeExit::Deadline, last_lines: String::new_in(alloc) }),
                        Fault::Stopped(_) => Ok(TreeRan::Failed { request, exit: TreeExit::Over, last_lines: String::new_in(alloc) }),
                        fault => Err(refused_words(&["the process tree was refused: ", &format!("{fault:?} ({:?})", error.rule)], alloc)),
                    };
                }
            };
            let exit = match ran.exit {
                Exit::Exited(0) => continue,
                Exit::Exited(code) => TreeExit::Code(code),
                Exit::Signaled(signal) => TreeExit::Signal(signal),
            };
            let lines = c_tasks_tree::last_lines(&ran.stderr, TREE_LINES, alloc);
            return Ok(TreeRan::Failed { request, exit, last_lines: to_guest(lines.as_str(), &shown, alloc) });
        }

        // 6. What the tree made or changed under `out`: a file equal to its laid bytes is the
        // state's, not the tree's; one holding a host folder is left out when it is a log or a
        // depfile (rule 25, c_tasks_tree.rs `left_out_holding_a_host_folder`), and any other,
        // or one the link asks for, is refused naming the file and the folder it holds.
        let listed = listed_under(out.as_str()).map_err(|e| fail(&["the tree's OUT_DIR could not be listed: ", &e.to_string()]))?;
        let demand = self.demand;
        let demanded = |name: &str| demand.iter().any(|asked| *asked == name);
        let mut kept: std::vec::Vec<crate::native_script::Listed> = std::vec::Vec::with_capacity(listed.len());
        let mut left_out = 0u32;
        for entry in listed {
            let name = entry.name.as_str();
            if entry.kind != bstd::fs::EntryKind::File {
                if demanded(name) {
                    return Err(fail(&["the process tree left `", name, "`, which the link asks for, as no plain file"]));
                }
                left_out += 1;
                continue;
            }
            let bytes = std::fs::read(format!("{}/{name}", out.as_str())).map_err(|e| fail(&[name, ": ", &e.to_string()]))?;
            let same = laid.binary_search_by(|(laid_name, _)| (*laid_name).cmp(name)).ok().and_then(|at| laid[at].1).is_some_and(|was| was == bytes.as_slice());
            if same {
                continue;
            }
            let host = shown.iter().map(|(_, host)| *host).find(|host| holds(&bytes, host.as_bytes())).or_else(|| holds(&bytes, folder.as_bytes()).then_some(folder.as_str()));
            if let Some(host) = host {
                if demanded(name) {
                    return Err(fail(&["the process tree's `", name, "`, which the link asks for, holds a folder of this machine (rule 25: equal inputs, equal bytes)"]));
                }
                // Only a log or a depfile is let go; any other is said, by name (rule 3).
                if !c_tasks_tree::left_out_holding_a_host_folder(name) {
                    return Ok(TreeRan::HostPath { file: String::from_str_in(name, alloc), host: String::from_str_in(host, alloc) });
                }
                left_out += 1;
                continue;
            }
            kept.push(entry);
        }
        let mut holder = Outputs::new_in(self.roots.out, alloc);
        store_out_dir(out.as_str(), setting.area, &kept, &mut holder, FileLimits { bytes: u64::MAX, files: c_tasks_tree::MOST_TREE_FILES }, alloc)
            .map_err(|e| fail(&["the tree's files could not be stored: ", &e.to_string()]))?;
        std::fs::remove_dir_all(folder.as_str()).map_err(|e| fail(&[folder.as_str(), ": ", &e.to_string()]))?;
        Ok(TreeRan::Made { files: core::mem::replace(&mut holder.stored, Vec::new_in(alloc)), left_out })
    }
}

#[cfg(test)]
mod tree_spelling_tests {
    use super::{shelf_program, to_guest, to_host};

    /// A guest folder is made its host folder where a word or a flag's letters begin with it,
    /// never inside a word or a longer name; shown again, the host folders are their guests.
    #[test]
    fn guest_folders_are_made_host_folders_at_word_starts_only() {
        let alloc = blaze_own::testing::heap().alloc();
        let pairs = [("/package", "/h/pkg"), ("/out", "/h/t/out")];
        let host = to_host("--prefix=/out -I/package/include /outer a/out /out/lib:/package CC=/out/x", &pairs, alloc);
        assert_eq!(host.as_str(), "--prefix=/h/t/out -I/h/pkg/include /outer a/out /h/t/out/lib:/h/pkg CC=/h/t/out/x");
        assert_eq!(to_host("/out", &pairs, alloc).as_str(), "/h/t/out", "a whole word");
        assert_eq!(to_host("x-/out", &pairs, alloc).as_str(), "x-/out", "CONTROL: a flag starts with '-'");
        let shown = to_guest("error: /h/t/out/src/a.c:1: no /h/pkg/b.h", &pairs, alloc);
        assert_eq!(shown.as_str(), "error: /out/src/a.c:1: no /package/b.h");
    }

    #[test]
    fn a_toolchain_word_is_the_shelf_member_of_its_name() {
        assert_eq!(shelf_program("/usr/bin/cc"), Some("bin/clang"));
        assert_eq!(shelf_program("g++"), Some("bin/clang++"));
        assert_eq!(shelf_program("ar"), Some("bin/llvm-ar"));
        assert_eq!(shelf_program("ccache"), None);
    }
}
