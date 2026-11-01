//! blaze's coverage-guided fuzzer, and the targets for every parser of untrusted bytes
//! (`targets/`, one binary each; fuzz/README.md lists them).
//!
//! The engine reads what SanitizerCoverage counts (`raw`): an inline 8-bit counter per block,
//! the PC table that says which block is a function's entry, and the operands of comparisons.
//! An input is kept when it reaches a new feature (a counter in a new AFL bucket of hit
//! counts, or a new value-profile bit), and the kept inputs are changed by libFuzzer's
//! mutations (`mutate`). A failing input is written out and can be minimized (`minimize`).
//! Fuzzing needs a build with SanitizerCoverage and AddressSanitizer; without them `fuzz`
//! stops naming the flags. `run`, `minimize` and `seeds` work in any build, and each target's
//! `#[test]` runs its seeds in a plain one.

#![feature(core_intrinsics, sanitize)]
#![allow(internal_features)]
#![deny(unsafe_code)]
// A command-line tool: it reports on stderr.
#![allow(clippy::print_stderr)]

mod corpus;
mod features;
mod minimize;
mod mutate;
#[allow(unsafe_code)]
mod raw;
#[cfg(test)]
mod tests;

// Every target binary links this crate, and `blaze_fuzz::largest_allocation` reads this
// allocator's counts (it refuses to measure without it).
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{
    collections::HashSet,
    io::{Read as _, Write as _},
    num::NonZero,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    corpus::{Corpus, Scratch},
    features::Seen,
    mutate::{Mutator, Traced},
    raw::Coverage,
};

/// What a fuzz build is given, in the names of rustc's flags (blaze takes them in `RUSTFLAGS`).
pub(crate) const FLAGS: &str = "build with `-Zsanitizer=address -Csanitizer-coverage-level=3 \
     -Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp` in RUSTFLAGS (fuzz/README.md)";

/// A fuzz target: one parser of untrusted bytes and what must hold of it on any input.
pub struct Target {
    /// The binary's name and its corpus directory, `fuzz/corpus/<name>/`.
    pub name: &'static str,
    /// Runs one input; panics when an invariant fails. Must return on any bytes, join every
    /// thread it starts, keep no state between calls that changes what it checks, and allow
    /// calls on several threads at once (the plain test runs on every core).
    pub check: fn(&[u8]),
    /// Valid inputs built through the writer side of the API.
    pub seeds: fn() -> Vec<Vec<u8>>,
    /// Tokens the mutator inserts (magic numbers, tags, field names), beside what the
    /// comparisons it sees teach it.
    pub dictionary: &'static [&'static [u8]],
    /// The longest input the mutator makes (the seeds may be longer).
    pub max_len: usize,
    /// How long CI fuzzes this target after merge.
    pub budget: Duration,
}

impl Target {
    /// `fuzz/corpus/<name>/`, the seeds in the source tree.
    pub fn corpus(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus").join(self.name)
    }

    /// `fuzz/out/<name>/`, where a fuzz run keeps what it found (ignored by git).
    fn out(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("out").join(self.name)
    }
}

/// The plain `#[test]` of a target: its corpus files, `seeds()`, and blaze_fuzz's
/// deterministic changes of both (20000 unless `BLAZE_FUZZ_RUNS` says), each through `check`,
/// on every core (`check` may run on several threads at once). The first failure stops the
/// rest.
pub fn seeds(target: &Target) {
    let inputs: Vec<Vec<u8>> = blaze_fuzz::inputs(target.corpus(), (target.seeds)()).collect();  // alloc: the plain test's inputs, setup
    let threads = std::thread::available_parallelism().map_or(1, NonZero::get).min(inputs.len()).max(1);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let _stop = StopOnPanic(&next, inputs.len());
                while let Some(input) = inputs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    (target.check)(input);
                }
            });
        }
    });
}

/// Which of `cases` ways to check an input (`cases` > 0) a target's run takes, when checking
/// each way on every run would repeat a whole read: set by the input's length and last byte,
/// so the same input takes the same way, and the engine's changes of it reach every way over
/// many runs.
pub fn case(data: &[u8], cases: usize) -> usize {
    (data.len() + usize::from(data.last().copied().unwrap_or(0))) % cases
}

/// The plain `#[test]` of a target whose `check` is `check_case` at one [`case`] of `cases`:
/// every case of each corpus file and of `seeds()`, so on them it checks what a `check` taking
/// every case on every run would. On every core, as [`seeds`]. An input with fewer ways than
/// `cases` is given the rest too, and checks only what every way does.
pub fn every_case(target: &Target, cases: usize, check_case: fn(&[u8], usize)) {
    let mut inputs = corpus::read_dir(&target.corpus()).unwrap_or_else(|e| panic!("{e}"));  // alloc: the plain test's inputs, setup
    inputs.extend((target.seeds)());
    let runs = inputs.len() * cases;
    let threads = std::thread::available_parallelism().map_or(1, NonZero::get).min(runs).max(1);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let _stop = StopOnPanic(&next, runs);
                loop {
                    let run = next.fetch_add(1, Ordering::Relaxed);
                    if run >= runs {
                        break;
                    }
                    check_case(&inputs[run / cases], run % cases);
                }
            });
        }
    });
}

/// Moves the shared index past the end when its thread panics, so the others stop.
struct StopOnPanic<'a>(&'a AtomicUsize, usize);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(self.1, Ordering::Relaxed);
        }
    }
}

const USAGE: &str = "usage: <target> fuzz [--jobs N] [--seconds N] [--runs N] [--seed N] [--max-len N] [--timeout-ms N] [--no-value-profile] [--out DIR]
       <target> run [--timeout-ms N] FILE...
       <target> minimize FILE [--seconds N] [--timeout-ms N] [--out DIR]
       <target> seeds
       <target> serve [--timeout-ms N]
FILE is an input in hexadecimal, as in fuzz/corpus/<target>/. `serve` is what `minimize` runs.";

/// The exit code of `run` and `serve` when an input ran past `--timeout-ms`.
pub(crate) const TIMED_OUT: i32 = 3;

enum Command {
    Fuzz,
    Run(Vec<PathBuf>),
    Minimize(PathBuf),
    Seeds,
    Serve,
}

struct Options {
    command: Command,
    seconds: Option<u64>,
    runs: Option<u64>,
    seed: Option<u64>,
    max_len: Option<usize>,
    timeout: Duration,
    value_profile: bool,
    out: Option<PathBuf>,
    /// Worker processes for `fuzz`, one per core given (the counters are a process's).
    jobs: usize,
    /// This process is worker `n` of a `--jobs` run: it reads what the others keep.
    worker: Option<usize>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut args = args.into_iter();
        let command = args.next().ok_or("no command")?;
        let mut options = Options {
            command: Command::Seeds,
            seconds: None,
            runs: None,
            seed: None,
            max_len: None,
            timeout: Duration::from_secs(10),
            value_profile: true,
            out: None,
            jobs: 1,
            worker: None,
        };
        let mut files = Vec::new();  // alloc: command-line parsing, once
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));  // alloc: error message
            let number = |name: &str, text: String| text.parse::<u64>().map_err(|e| format!("{name} {text}: {e}"));  // alloc: error message
            match arg.as_str() {
                "--seconds" => options.seconds = Some(number(&arg, value(&arg)?)?),
                "--runs" => options.runs = Some(number(&arg, value(&arg)?)?),
                "--seed" => options.seed = Some(number(&arg, value(&arg)?)?),
                "--max-len" => options.max_len = Some(number(&arg, value(&arg)?)? as usize),
                "--timeout-ms" => options.timeout = Duration::from_millis(number(&arg, value(&arg)?)?),
                "--no-value-profile" => options.value_profile = false,
                "--out" => options.out = Some(PathBuf::from(value(&arg)?)),
                "--jobs" => match number(&arg, value(&arg)?)? {
                    0 => return Err("--jobs 0: at least one".to_owned()),  // alloc: error message
                    n => options.jobs = n as usize,
                },
                "--worker" => options.worker = Some(number(&arg, value(&arg)?)? as usize),
                flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),  // alloc: error message
                _ => files.push(PathBuf::from(arg)),
            }
        }
        options.command = match command.as_str() {
            "fuzz" | "seeds" | "serve" if !files.is_empty() => return Err(format!("`{command}` takes no files")),  // alloc: error message
            "fuzz" => Command::Fuzz,
            "seeds" => Command::Seeds,
            "serve" => Command::Serve,
            "run" if files.is_empty() => return Err("`run` needs a file".to_owned()),  // alloc: error message
            "run" => Command::Run(files),
            "minimize" if files.len() == 1 => Command::Minimize(files.remove(0)),
            "minimize" => return Err("`minimize` takes one file".to_owned()),  // alloc: error message
            other => return Err(format!("unknown command {other:?}")),  // alloc: error message
        };
        Ok(options)
    }
}

/// A target binary's `main`.
pub fn main(target: &Target) -> ExitCode {
    raw::keep();
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{}: {e}\n{USAGE}", target.name);
            return ExitCode::from(2);
        }
    };
    let out = options.out.clone().unwrap_or_else(|| target.out());  // clone: the output path, once at start
    let done = match &options.command {
        Command::Fuzz if options.jobs > 1 => jobs(&options, &out),
        Command::Fuzz => fuzz(target, &options, &out),
        Command::Run(files) => run(target, files, options.timeout),
        Command::Serve => serve(target, options.timeout),
        Command::Minimize(file) => minimize::minimize(
            file,
            &out,
            Duration::from_secs(options.seconds.unwrap_or(60)),
            options.timeout,
        ),
        Command::Seeds => {
            seeds(target);
            eprintln!("{}: the seeds and their changes pass", target.name);
            Ok(ExitCode::SUCCESS)
        }
    };
    done.unwrap_or_else(|e| {
        eprintln!("{}: error: {e}", target.name);
        ExitCode::from(2)
    })
}

/// Each file once, through `check`, with nothing caught: a failure ends the process as it
/// would anywhere, and an input that runs past `timeout` ends it with [`TIMED_OUT`].
fn run(target: &Target, files: &[PathBuf], timeout: Duration) -> Result<ExitCode, String> {
    timer(timeout)?;
    for file in files {
        let input = corpus::read(file)?;
        timed(|| (target.check)(&input));
        eprintln!("{}: {} passes ({} bytes)", target.name, file.display(), input.len());
    }
    Ok(ExitCode::SUCCESS)
}

/// What `minimize` runs, so that a try costs a pipe's round trip rather than a process: each
/// input is its length (`u32`, little-endian) and its bytes on stdin, and each gets one byte
/// on stdout, 0 when it passed and 1 when `check` panicked. It ends at the end of stdin. A
/// failure that ends the process (an AddressSanitizer report, [`TIMED_OUT`]) ends it as in
/// `run`.
fn serve(target: &Target, timeout: Duration) -> Result<ExitCode, String> {
    timer(timeout)?;
    let (mut stdin, mut stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    let mut input = Vec::new();  // alloc: serve's input buffer, reused for every input
    loop {
        let mut len = [0u8; 4];
        match stdin.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(ExitCode::SUCCESS),
            Err(e) => return Err(format!("stdin: {e}")),  // alloc: error message
        }
        input.resize(u32::from_le_bytes(len) as usize, 0);
        stdin.read_exact(&mut input).map_err(|e| format!("stdin: {e}"))?;  // alloc: error message
        let passed = timed(|| catch_unwind(AssertUnwindSafe(|| (target.check)(&input))).is_ok());
        stdout.write_all(&[u8::from(!passed)]).and_then(|()| stdout.flush()).map_err(|e| format!("stdout: {e}"))?;  // alloc: error message
    }
}

/// The millisecond the input `run` or `serve` is running started at, plus one; 0 between.
static RUNNING_SINCE: AtomicU64 = AtomicU64::new(0);

fn timed<T>(f: impl FnOnce() -> T) -> T {
    RUNNING_SINCE.store(now_ms() + 1, Ordering::Release);
    let it = f();
    RUNNING_SINCE.store(0, Ordering::Release);
    it
}

/// For `run` and `serve`: a thread that ends the process with [`TIMED_OUT`] when an input
/// runs past `timeout`.
fn timer(timeout: Duration) -> Result<(), String> {
    let limit = timeout.as_millis() as u64;
    now_ms();
    std::thread::Builder::new()
        .name("timer".to_owned())  // alloc: a thread's name, once at start
        .spawn(move || {
            loop {
                #[allow(clippy::disallowed_methods, reason = "the input's limit on the wall clock: what runs past it says nothing")]
                std::thread::sleep(Duration::from_millis((limit / 4).clamp(10, 1000)));
                let since = RUNNING_SINCE.load(Ordering::Acquire);
                if since != 0 && now_ms().saturating_sub(since - 1) > limit {
                    eprintln!("blaze-fuzzer: an input ran past {limit} ms");
                    std::process::exit(TIMED_OUT);
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("the timer: {e}"))  // alloc: error message
}

/// `fuzz --jobs N`: N processes of this binary's `fuzz`, worker `k` seeded `seed + k`, which
/// share what they keep through `out/corpus/`. It ends when they all have, or at the first
/// that fails, stopping the rest, with that one's exit code.
fn jobs(options: &Options, out: &Path) -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary's path: {e}"))?;  // alloc: error message
    let seed = options.seed.unwrap_or_else(clock_seed);
    let mut workers = Vec::with_capacity(options.jobs);
    for worker in 0..options.jobs {
        let mut command = std::process::Command::new(&exe);
        command.arg("fuzz").args(["--worker", &worker.to_string()]);  // alloc: a worker's command line, once per worker
        command.args(["--seed", &seed.wrapping_add(worker as u64).to_string()]);  // alloc: a worker's command line, once per worker
        command.args(["--timeout-ms", &options.timeout.as_millis().to_string()]);  // alloc: a worker's command line, once per worker
        command.arg("--out").arg(out);
        if let Some(seconds) = options.seconds {
            command.args(["--seconds", &seconds.to_string()]);  // alloc: a worker's command line, once per worker
        }
        if let Some(runs) = options.runs {
            command.args(["--runs", &runs.to_string()]);  // alloc: a worker's command line, once per worker
        }
        if let Some(max_len) = options.max_len {
            command.args(["--max-len", &max_len.to_string()]);  // alloc: a worker's command line, once per worker
        }
        if !options.value_profile {
            command.arg("--no-value-profile");
        }
        match command.spawn() {
            Ok(child) => workers.push(child),
            Err(e) => {
                stop(&mut workers);
                return Err(format!("worker {worker}: {e}"));  // alloc: error message
            }
        }
    }
    loop {
        let mut running = 0;
        for worker in 0..workers.len() {
            match workers[worker].try_wait() {
                Ok(None) => running += 1,
                Ok(Some(status)) if status.success() => {}
                Ok(Some(status)) => {
                    stop(&mut workers);
                    eprintln!("blaze-fuzzer: worker {worker} ended with {status}");
                    return Ok(status.code().map_or(ExitCode::FAILURE, |code| ExitCode::from(code as u8)));
                }
                Err(e) => {
                    stop(&mut workers);
                    return Err(format!("worker {worker}: {e}"));  // alloc: error message
                }
            }
        }
        if running == 0 {
            return Ok(ExitCode::SUCCESS);
        }
        #[allow(clippy::disallowed_methods, reason = "std waits on one child, not the first of many, and the rest must stay killable")]
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop(workers: &mut [std::process::Child]) {
    for worker in workers {
        _ = worker.kill();
        _ = worker.wait();
    }
}

fn clock_seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_nanos() as u64)
}

/// What ended a run.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Finding {
    Crash,
    Timeout,
}

static OUT: OnceLock<PathBuf> = OnceLock::new();
static EPOCH: OnceLock<Instant> = OnceLock::new();
static DUMPED: AtomicBool = AtomicBool::new(false);
static NAME: OnceLock<&'static str> = OnceLock::new();

fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Writes the input being run out, from AddressSanitizer's error hook or the watchdog, which
/// end the process.
pub(crate) fn dump(finding: Finding) {
    if let Some(input) = raw::current() {
        report(finding, &input);
    }
}

/// Writes `input` to `out/<crash|timeout>-<hash>.hex`, once per process, and says how to
/// replay and minimize it.
fn report(finding: Finding, input: &[u8]) {
    let Some(out) = OUT.get() else {
        return;
    };
    if DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let prefix = match finding {
        Finding::Crash => "crash-",
        Finding::Timeout => "timeout-",
    };
    let name = NAME.get().copied().unwrap_or("<target>");
    match corpus::write(out, prefix, input) {
        Ok(path) => eprintln!(
            "blaze-fuzzer: {finding:?} on {} bytes, written to {}\n  replay:   {name} run {}\n  minimize: {name} minimize {}",
            input.len(),
            path.display(),
            path.display(),
            path.display()
        ),
        Err(e) => eprintln!("blaze-fuzzer: {finding:?}, and its input could not be written: {e}"),
    }
}

/// Ends the process, writing the input out, when one runs past `timeout` milliseconds. Its
/// counters are the engine's, not the target's: `Coverage::new` drops them.
#[inline(never)]
fn watchdog(timeout: u64) {
    loop {
        #[allow(clippy::disallowed_methods, reason = "the input's limit on the wall clock: what runs past it says nothing")]
        std::thread::sleep(Duration::from_millis((timeout / 4).clamp(10, 1000)));
        if raw::claim_timed_out(now_ms(), timeout) {
            eprintln!("blaze-fuzzer: an input ran past {timeout} ms");
            dump(Finding::Timeout);
            std::process::abort();
        }
    }
}

/// The fuzz loop: until the time budget or the count of runs is spent, or a run fails.
fn fuzz(target: &Target, options: &Options, out: &Path) -> Result<ExitCode, String> {
    let coverage = Coverage::new(&[watchdog as usize, dump as usize, now_ms as usize])?;
    if !raw::asan() {
        return Err(format!("this binary was built without AddressSanitizer: {FLAGS}"));  // alloc: error message
    }
    if options.value_profile && !raw::return_address_works() {
        return Err(
            "the code generator gives a null `core::intrinsics::return_address`, which the value profile needs to \
             tell comparisons apart; give --no-value-profile to fuzz without it"
                .to_owned(),  // alloc: error message
        );
    }
    let kept = out.join("corpus");
    std::fs::create_dir_all(&kept).map_err(|e| format!("{}: {e}", kept.display()))?;  // alloc: error message
    _ = OUT.set(out.to_owned());  // alloc: the output path, once at start
    _ = NAME.set(target.name);
    now_ms();
    let timeout = options.timeout.as_millis() as u64;
    std::thread::Builder::new()
        .name("watchdog".to_owned())  // alloc: a thread's name, once at start
        .spawn(move || watchdog(timeout))
        .map_err(|e| format!("the watchdog: {e}"))?;  // alloc: error message

    let seed = options.seed.unwrap_or_else(clock_seed);
    let budget = Duration::from_secs(options.seconds.unwrap_or(target.budget.as_secs()));
    let deadline = Instant::now() + budget;
    let runs = options.runs.unwrap_or(u64::MAX);
    let mut fuzzer = Fuzzer {
        target,
        coverage: &coverage,
        seen: Seen::new((coverage.total() << 3) + raw::VALUE_BITS),
        corpus: Corpus::new().map_err(|e| format!("the corpus: {e}"))?,  // alloc: error message
        features: Vec::with_capacity(coverage.total() + raw::VALUE_BITS),
        value_profile: options.value_profile,
        runs: 0,
        started: Instant::now(),
        worker: options.worker,
        kept,
        known: HashSet::with_capacity(KNOWN),  // alloc: setup
        scratch: Scratch::default(),
    };
    eprintln!(
        "{}: seed {seed}, {} counters, {budget:?}, out {}",
        fuzzer.name(),
        coverage.total(),
        out.display()
    );

    // What earlier runs (and other workers) kept, then the empty input, the corpus files and
    // the seeds built in code, which are kept too when they reach something new.
    if let Some(failed) = fuzzer.sync()? {
        return Ok(failed);
    }
    let mut initial = vec![Vec::new()];  // alloc: the initial inputs, setup
    initial.extend(corpus::read_dir(&target.corpus())?);
    initial.extend((target.seeds)());
    for input in &initial {
        if let Tried::Failed = fuzzer.try_input(input, now_ms())? {
            return Ok(fuzzer.failed());
        }
    }
    if fuzzer.corpus.len() == 0 {
        return Err(format!("no input reached a counter: `{}` itself was built without coverage", target.name));  // alloc: error message
    }
    fuzzer.status("loaded");

    let max_len = options.max_len.unwrap_or(target.max_len);
    let mut mutator = Mutator::new(seed, target.dictionary, Traced);
    let mut data = Vec::with_capacity(max_len);
    let mut next_status = 1u64;
    let mut last_status = Instant::now();
    let mut last_sync = last_status;
    while fuzzer.runs < runs {
        // The one clock read of a run: the deadline, the watchdog's start and the pulses.
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        data.clear();
        data.extend_from_slice(fuzzer.corpus.pick(&mut mutator.random));
        let other = fuzzer.corpus.pick(&mut mutator.random);
        mutator.mutate(&mut data, other, max_len);
        if let Tried::Failed = fuzzer.try_input(&data, ms_at(now))? {
            return Ok(fuzzer.failed());
        }
        if fuzzer.worker.is_some() && now - last_sync > SYNC {
            if let Some(failed) = fuzzer.sync()? {
                return Ok(failed);
            }
            last_sync = now;
        }
        if fuzzer.runs >= next_status || now - last_status > Duration::from_secs(10) {
            fuzzer.status("pulse");
            next_status = fuzzer.runs.saturating_mul(2);
            last_status = now;
        }
    }
    fuzzer.status("done");
    Ok(ExitCode::SUCCESS)
}

/// A target's name in a status line, with the worker's number in a `--jobs` run.
struct Name(&'static str, Option<usize>);

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.1 {
            Some(worker) => write!(f, "{}[{worker}]", self.0),
            None => f.write_str(self.0),
        }
    }
}

/// How often a `--jobs` worker reads what the others kept (libFuzzer's `-reload` default).
const SYNC: Duration = Duration::from_secs(1);

fn ms_at(now: Instant) -> u64 {
    now.saturating_duration_since(*EPOCH.get_or_init(Instant::now)).as_millis() as u64
}

struct Fuzzer<'a> {
    target: &'a Target,
    coverage: &'a Coverage<'static>,
    seen: Seen,
    corpus: Corpus,
    features: Vec<u32>,
    value_profile: bool,
    runs: u64,
    started: Instant,
    worker: Option<usize>,
    /// `out/corpus/`, what this run and others keep.
    kept: PathBuf,
    /// The files in `kept` this process has read or written, by [`corpus::key`].
    known: HashSet<u64>,
    /// What a kept input's file is written from.
    scratch: Scratch,
}

/// How many kept files `known` holds before it grows.
const KNOWN: usize = 1 << 14;

enum Tried {
    Failed,
    /// It reached something new: the corpus has a copy now, and `kept` its file.
    Kept,
    Dropped,
}

impl Fuzzer<'_> {
    /// Runs `input`, started at millisecond `now`.
    fn try_input(&mut self, input: &[u8], now: u64) -> Result<Tried, String> {
        self.features.clear();
        let passed = self.coverage.run(self.target.check, input, now);
        self.coverage.drain(&mut self.features, self.value_profile);
        self.runs += 1;
        if !passed {
            // A panic that left `check`; one the code under test caught itself is no finding.
            report(Finding::Crash, input);
            return Ok(Tried::Failed);
        }
        let new = self.seen.merge(&self.features);
        if new == 0 {
            return Ok(Tried::Dropped);
        }
        if self.known.insert(corpus::hash(input)) {  // alloc: grows past KNOWN kept files only, by doubling
            self.scratch.write(&self.kept, "", input)?;
        }
        self.corpus.add(input, new).map_err(|e| format!("keeping an input of {} bytes: {e}", input.len()))?;  // alloc: error message
        Ok(Tried::Kept)
    }

    /// Runs each input in `kept` this process has not seen: at the start, and every [`SYNC`]
    /// in a `--jobs` worker. One directory listing; only new files are read.
    fn sync(&mut self) -> Result<Option<ExitCode>, String> {
        for name in corpus::names(&self.kept)? {
            if !self.known.insert(corpus::key(&name)) {  // alloc: grows past KNOWN kept files only, by doubling
                continue;
            }
            let input = corpus::read(&self.kept.join(&name))?;  // alloc: a file another worker kept, read once
            if let Tried::Failed = self.try_input(&input, now_ms())? {
                return Ok(Some(self.failed()));
            }
        }
        Ok(None)
    }

    fn name(&self) -> Name {
        Name(self.target.name, self.worker)
    }

    fn failed(&self) -> ExitCode {
        // `try_input` wrote the input out; an AddressSanitizer report never returns here.
        eprintln!("{}: failed after {} runs", self.name(), self.runs);
        ExitCode::FAILURE
    }

    fn status(&self, what: &str) {
        let seconds = self.started.elapsed().as_secs_f64().max(1e-3);
        eprintln!(
            "{} #{} {what} features: {} corpus: {} ({} bytes) exec/s: {:.0}",
            self.name(),
            self.runs,
            self.seen.count(),
            self.corpus.len(),
            self.corpus.bytes(),
            self.runs as f64 / seconds
        );
    }
}
