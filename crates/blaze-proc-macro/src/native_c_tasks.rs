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
//! A process-tree request (tikv-jemalloc-sys's `sh configure`, `make`) is refused by name for
//! now ([`TasksError::Trees`]): its task is the next piece.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use std::path::Path;

use blaze_build_script::c_tasks::{self, CCompiler, CStore, Compiled, FileRef, GuestRoots, Looked, Made, PlanError, Request, TaskError, Toolchain};
use blaze_build_script::c_tasks_built::Report;
use blaze_build_script::outputs::OutputsRef;
use blaze_own::{AString, AVec, Alloc, Heap, SharedAlloc, String, Vec};
use bstd::procs::hermetic::{Hermetic, HermeticCapOpts};

use crate::native_c::{DiskFacts, FileArea, GuestToday, Laid, ShelfClang};
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
    /// Process-tree requests, not run yet.
    Trees { package: &'b str, requests: u32 },
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
            TasksError::Trees { package, requests } => write!(
                f,
                "the C of {package}: {requests} process-tree requests (`sh configure`, `make`) are not run yet (native_c_tasks.rs): its archives are not built"
            ),
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
    store: &mut dyn CStore,
    alloc: Alloc<'a>,
) -> Result<TasksRun<'a, 'b>, TasksError<'a, 'b>> {
    let no_worker = |words: &str| TasksError::Worker { package, words: String::from_str_in(words, alloc) };
    let (world, _) = native_world(Path::new(setting.work)).map_err(|e| no_worker(&e.to_string()))?;
    let clock = world.need_clock().map_err(|missing| no_worker(missing.capability))?;
    let started = clock.now().since_start();

    let plan = c_tasks::plan(outputs, &roots, demand, alloc).map_err(|why| TasksError::Task(TaskError::Plan { package, why }))?;
    if !plan.trees.is_empty() {
        return Err(TasksError::Trees { package, requests: plan.trees.len() as u32 });
    }
    let c_tasks::Plan { units, planned, archives, counts, .. } = plan;
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
    Ok(TasksRun { made, report })
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
