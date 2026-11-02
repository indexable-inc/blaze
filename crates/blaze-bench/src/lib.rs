//! blaze's paired benchmark runner, for the benchmarks in `benches/`:
//!
//! - `solo [-t SECONDS | -s SAMPLES] [-f GLOB]` measures this binary's benchmarks alone;
//! - `compare BASELINE [-t SECONDS | -s SAMPLES] [--noise-threshold PERCENT] [-o] [-f GLOB]
//!   [--seed N]` pairs each benchmark of this binary (the candidate) with the one of the same
//!   name in `BASELINE`, the binary of the change's base, and fails when one is slower beyond
//!   doubt.
//!
//! A comparison runs both binaries as workers (`worker`, requests on stdin, one answer a line on
//! stdout) and takes their samples in turns, in an order drawn from the seed each time, so drift
//! of the machine reaches both sides of a pair. Its statistic is tango's (tango-bench 0.9, which
//! this replaced): each pair's difference per iteration, a z-score of their mean over its
//! standard error, significant at |z| >= 2.6 (99%) and above the noise threshold; `-o` drops the
//! pairs outside Tukey's fences (1.5 interquartile ranges) first.
//!
//! Every benchmark does the same work on every run, so that the binaries of a change and of its
//! base can be paired; a benchmark writes nothing to stdout, which carries a worker's answers.

use std::fmt;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime};

use blaze_rng::SplitMix64;

/// What `protocol` prints: a baseline must speak the same version as the candidate.
pub const PROTOCOL: &str = "blaze-bench-protocol 1";

/// The time a sample is sized to take.
const TIME_SLICE: Duration = Duration::from_millis(10);

/// The budget of a benchmark when neither `-t` nor `-s` is given.
const DEFAULT_TIME: Duration = Duration::from_millis(100);

/// The |z-score| a difference must reach to be significant: 99%.
const Z_SIGNIFICANT: f64 = 2.6;

/// A named benchmark, its state made only when it is run.
pub struct Benchmark {
    name: String,
    make: Box<dyn FnMut(Bencher) -> Sampler>,
}

/// `make` builds the benchmark's state and returns [`Bencher::iter`] of the work.
pub fn benchmark_fn(
    name: impl Into<String>,
    make: impl FnMut(Bencher) -> Sampler + 'static,
) -> Benchmark {
    let name = name.into();
    assert!(!name.is_empty() && !name.contains('\n'), "a benchmark name is one non-empty line");
    Benchmark { name, make: Box::new(make) }
}

/// Handed to a benchmark's `make`, which ends with [`Bencher::iter`].
pub struct Bencher(());

impl Bencher {
    /// The sampler running `work` once an iteration.
    pub fn iter<O>(self, mut work: impl FnMut() -> O + 'static) -> Sampler {
        Sampler(Box::new(move |iterations| {
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(work());
            }
            start.elapsed().as_nanos() as u64
        }))
    }
}

/// A benchmark's work, timed over a number of iterations in nanoseconds.
pub struct Sampler(Box<dyn FnMut(u64) -> u64>);

impl Sampler {
    fn measure(&mut self, iterations: u64) -> u64 {
        (self.0)(iterations)
    }

    /// The iterations that take about `slice`, found in up to five rounds that stop once an
    /// estimate is within twice the last; a round is taken as at least 1 µs, so a too-short
    /// measurement cannot ask for an unbounded count.
    fn estimate(&mut self, slice: Duration) -> u64 {
        let slice = slice.as_nanos() as u64;
        let mut iterations = 1;
        for _ in 0..5 {
            let per_iteration = (self.measure(iterations).max(1_000) / iterations).max(1);
            let next = slice / per_iteration;
            if next < 2 * iterations {
                return next.max(1);
            }
            iterations = next;
        }
        iterations.max(1)
    }
}

/// A list of benchmarks.
pub trait IntoBenchmarks {
    fn into_benchmarks(self) -> Vec<Benchmark>;
}

impl<const N: usize> IntoBenchmarks for [Benchmark; N] {
    fn into_benchmarks(self) -> Vec<Benchmark> {
        self.into()
    }
}

impl IntoBenchmarks for Vec<Benchmark> {
    fn into_benchmarks(self) -> Vec<Benchmark> {
        self
    }
}

/// The `main` of a benchmark binary (`harness = false`) running `$benchmarks`.
#[macro_export]
macro_rules! main {
    ($benchmarks:expr) => {
        fn main() -> ::std::process::ExitCode {
            $crate::run($crate::IntoBenchmarks::into_benchmarks($benchmarks))
        }
    };
}

/// Runs the command on this process's arguments.
pub fn run(benchmarks: Vec<Benchmark>) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Mode::parse(&args).and_then(|command| command.run(benchmarks)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("blaze-bench: {error}");
            ExitCode::from(2)
        }
    }
}

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy)]
enum Budget {
    Time(Duration),
    Samples(usize),
}

impl Budget {
    fn more(self, samples: usize, spent: Duration) -> bool {
        match self {
            Budget::Time(time) => spent < time,
            Budget::Samples(count) => samples < count,
        }
    }
}

struct Options {
    budget: Budget,
    filter: String,
    noise_threshold: f64,
    filter_outliers: bool,
    seed: Option<u64>,
}

enum Mode {
    Solo(Options),
    Compare(PathBuf, Options),
    Worker,
    Protocol,
}

impl Mode {
    fn parse(args: &[String]) -> Result<Mode> {
        let mut args = args.iter().map(String::as_str);
        let mut command = None;
        let mut baseline = None;
        let (mut time, mut samples) = (None, None);
        let mut options = Options {
            budget: Budget::Time(DEFAULT_TIME),
            filter: String::new(),
            noise_threshold: 1.0,
            filter_outliers: false,
            seed: None,
        };
        while let Some(arg) = args.next() {
            let mut value =
                |flag: &str| args.next().ok_or_else(|| format!("`{flag}` takes a value"));
            match arg {
                // What `cargo bench` passes a `harness = false` binary.
                "--bench" => {}
                // Output is never colored.
                "--color" => {
                    value(arg)?;
                }
                "-t" | "--time" => time = Some(number::<f64>(arg, value(arg)?)?),
                "-s" | "--samples" => samples = Some(number::<usize>(arg, value(arg)?)?),
                "-f" | "--filter" => options.filter = value(arg)?.to_owned(),
                "--noise-threshold" => options.noise_threshold = number(arg, value(arg)?)?,
                "--seed" => options.seed = Some(number(arg, value(arg)?)?),
                "-o" | "--filter-outliers" => options.filter_outliers = true,
                "solo" | "compare" | "worker" | "protocol" if command.is_none() => {
                    command = Some(arg)
                }
                _ if command == Some("compare") && baseline.is_none() && !arg.starts_with('-') => {
                    baseline = Some(PathBuf::from(arg));
                }
                _ => return Err(format!("unknown argument `{arg}`; {USAGE}")),
            }
        }
        options.budget = match (time, samples) {
            (Some(_), Some(_)) => return Err("`-t` and `-s` exclude each other".to_owned()),
            (Some(time), None) if time > 0.0 => Budget::Time(Duration::from_secs_f64(time)),
            (Some(time), None) => return Err(format!("`-t {time}` is not a positive time")),
            (None, Some(0)) => return Err("`-s 0` takes no samples".to_owned()),
            (None, Some(count)) => Budget::Samples(count),
            (None, None) => Budget::Time(DEFAULT_TIME),
        };
        match command {
            Some("solo") => Ok(Mode::Solo(options)),
            Some("compare") => {
                let baseline =
                    baseline.ok_or_else(|| format!("`compare` names its baseline; {USAGE}"))?;
                Ok(Mode::Compare(baseline, options))
            }
            Some("worker") => Ok(Mode::Worker),
            Some("protocol") => Ok(Mode::Protocol),
            _ => Err(format!("no command; {USAGE}")),
        }
    }

    fn run(self, benchmarks: Vec<Benchmark>) -> Result<ExitCode> {
        match self {
            Mode::Solo(options) => solo(benchmarks, &options),
            Mode::Compare(baseline, options) => compare(&baseline, &options),
            Mode::Worker => worker(benchmarks),
            Mode::Protocol => {
                println!("{PROTOCOL}");
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

const USAGE: &str = "usage: solo [-t SECONDS | -s SAMPLES] [-f GLOB], or compare BASELINE [-t SECONDS | \
                     -s SAMPLES] [--noise-threshold PERCENT] [-o] [-f GLOB] [--seed N]";

fn number<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T> {
    value.parse().map_err(|_| format!("`{flag} {value}` is not a number"))
}

fn solo(mut benchmarks: Vec<Benchmark>, options: &Options) -> Result<ExitCode> {
    for benchmark in benchmarks.iter_mut().filter(|it| glob(&options.filter, &it.name)) {
        let mut sampler = (benchmark.make)(Bencher(()));
        let iterations = sampler.estimate(TIME_SLICE);
        let mut per_iteration = Vec::new();
        let mut spent = Duration::ZERO;
        while options.budget.more(per_iteration.len(), spent) {
            let ns = sampler.measure(iterations);
            per_iteration.push(ns as f64 / iterations as f64);
            spent += Duration::from_nanos(ns);
        }
        let it = Summary::of(&per_iteration)
            .ok_or_else(|| format!("`{}` took no samples", benchmark.name))?;
        println!(
            "{:50}  [ {:>8} ... {:>8} ... {:>8} ]  stddev: {:>8}",
            benchmark.name,
            Human(it.min),
            Human(it.mean),
            Human(it.max),
            Human(it.variance.sqrt()),
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn compare(baseline: &Path, options: &Options) -> Result<ExitCode> {
    let candidate =
        std::env::current_exe().map_err(|error| format!("this binary's path: {error}"))?;
    let mut c = Worker::spawn(&candidate)?;
    let mut b = Worker::spawn(baseline)?;
    let (c_names, b_names) = (c.list()?, b.list()?);
    let seed = options.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |it| it.as_nanos() as u64)
    });
    let mut order = SplitMix64::new(seed);
    let mut code = ExitCode::SUCCESS;
    for (c_index, name) in c_names.iter().enumerate().filter(|(_, it)| glob(&options.filter, it)) {
        let Some(b_index) = b_names.iter().position(|it| it == name) else { continue };
        c.ask(&format!("prepare {c_index}"))?;
        b.ask(&format!("prepare {b_index}"))?;
        let slice = TIME_SLICE.as_nanos();
        let iterations = c
            .number(&format!("estimate {slice}"))?
            .min(b.number(&format!("estimate {slice}"))?)
            .max(1);
        let measure = format!("measure {iterations}");
        let mut pairs = Vec::new();
        let start = Instant::now();
        while options.budget.more(pairs.len(), start.elapsed()) {
            let pair = if order.coin() {
                let b_ns = b.number(&measure)?;
                (b_ns, c.number(&measure)?)
            } else {
                let c_ns = c.number(&measure)?;
                (b.number(&measure)?, c_ns)
            };
            pairs.push(pair);
        }
        c.ask("drop")?;
        b.ask("drop")?;
        let it = Paired::of(&pairs, iterations, options)
            .ok_or_else(|| format!("`{name}` took no samples"))?;
        println!(
            "{:50} [ {:>8} ... {:>8} ]    {:>+7.2}%{}",
            name,
            Human(it.baseline_mean),
            Human(it.candidate_mean),
            it.percent,
            if it.significant { "*" } else { "" },
        );
        if it.significant && it.percent > 0.0 {
            code = ExitCode::FAILURE;
        }
    }
    Ok(code)
}

/// Answers the requests on stdin, one a line, until it closes: `list` (the count, then each
/// name), `prepare INDEX`, `estimate NANOSECONDS` (iterations), `measure ITERATIONS`
/// (nanoseconds) and `drop`.
fn worker(mut benchmarks: Vec<Benchmark>) -> Result<ExitCode> {
    let mut sampler: Option<Sampler> = None;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|error| format!("a worker's stdin: {error}"))?;
        let (verb, arg) = line.split_once(' ').unwrap_or((&line, ""));
        let prepared = |sampler: &mut Option<Sampler>| {
            sampler.take().ok_or_else(|| format!("`{line}` before `prepare`"))
        };
        let answer = match verb {
            "list" => {
                let mut answer = benchmarks.len().to_string();
                for benchmark in &benchmarks {
                    answer.push('\n');
                    answer.push_str(&benchmark.name);
                }
                answer
            }
            "prepare" => {
                let index: usize = number(verb, arg)?;
                let benchmark =
                    benchmarks.get_mut(index).ok_or_else(|| format!("no benchmark {index}"))?;
                sampler = Some((benchmark.make)(Bencher(())));
                "ok".to_owned()
            }
            "estimate" | "measure" => {
                let value: u64 = number(verb, arg)?;
                let mut it = prepared(&mut sampler)?;
                let answer = if verb == "estimate" {
                    it.estimate(Duration::from_nanos(value))
                } else {
                    it.measure(value)
                };
                sampler = Some(it);
                answer.to_string()
            }
            "drop" => {
                sampler = None;
                "ok".to_owned()
            }
            _ => return Err(format!("a worker got `{line}`")),
        };
        writeln!(out, "{answer}")
            .and_then(|()| out.flush())
            .map_err(|error| format!("a worker's stdout: {error}"))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// A benchmark binary running as a worker, killed when dropped.
struct Worker {
    path: PathBuf,
    child: Child,
    to: ChildStdin,
    from: BufReader<ChildStdout>,
}

impl Worker {
    fn spawn(path: &Path) -> Result<Worker> {
        let shown = path.display();
        let spoken = Command::new(path)
            .arg("protocol")
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| format!("running {shown}: {error}"))?;
        let spoken = String::from_utf8_lossy(&spoken.stdout);
        if spoken.trim() != PROTOCOL {
            return Err(format!(
                "{shown} does not speak {PROTOCOL} (it answered `protocol` with {spoken:?})"
            ));
        }
        let mut child = Command::new(path)
            .arg("worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("running {shown}: {error}"))?;
        let to = child.stdin.take().expect("a piped stdin");
        let from = BufReader::new(child.stdout.take().expect("a piped stdout"));
        Ok(Worker { path: path.to_owned(), child, to, from })
    }

    fn line(&mut self) -> Result<String> {
        let mut line = String::new();
        match self.from.read_line(&mut line) {
            Ok(0) => Err(format!("{} stopped answering", self.path.display())),
            Ok(_) => Ok(line.trim_end_matches('\n').to_owned()),
            Err(error) => Err(format!("reading {}: {error}", self.path.display())),
        }
    }

    fn ask(&mut self, request: &str) -> Result<String> {
        writeln!(self.to, "{request}")
            .and_then(|()| self.to.flush())
            .map_err(|error| format!("asking {} `{request}`: {error}", self.path.display()))?;
        self.line()
    }

    fn number(&mut self, request: &str) -> Result<u64> {
        let answer = self.ask(request)?;
        answer
            .parse()
            .map_err(|_| format!("{} answered `{request}` with `{answer}`", self.path.display()))
    }

    fn list(&mut self) -> Result<Vec<String>> {
        let count = self.number("list")?;
        (0..count).map(|_| self.line()).collect()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `*` matches any run of characters and `?` one; the empty pattern matches every name.
fn glob(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|at| matches(rest, &name[at..])),
            Some((&first, rest)) => name
                .split_first()
                .is_some_and(|(&it, name)| (first == '?' || first == it) && matches(rest, name)),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    pattern.is_empty() || matches(&pattern, &name.chars().collect::<Vec<_>>())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Summary {
    n: usize,
    min: f64,
    max: f64,
    mean: f64,
    /// The sample variance (n - 1).
    variance: f64,
}

impl Summary {
    /// Welford's one pass, `None` for no values.
    fn of(values: &[f64]) -> Option<Summary> {
        let (&first, _) = values.split_first()?;
        let mut it = Summary { n: 0, min: first, max: first, mean: 0.0, variance: 0.0 };
        let mut squares = 0.0;
        for &value in values {
            it.n += 1;
            it.min = it.min.min(value);
            it.max = it.max.max(value);
            let before = it.mean;
            it.mean += (value - before) / it.n as f64;
            squares += (value - before) * (value - it.mean);
        }
        it.variance = if it.n > 1 { squares / (it.n - 1) as f64 } else { 0.0 };
        Some(it)
    }
}

/// The comparison of a benchmark's pairs.
#[derive(Debug)]
struct Paired {
    baseline_mean: f64,
    candidate_mean: f64,
    /// The mean difference against the baseline's mean: above 0 is slower.
    percent: f64,
    significant: bool,
}

impl Paired {
    /// From `(baseline, candidate)` nanoseconds of `iterations` each.
    fn of(pairs: &[(u64, u64)], iterations: u64, options: &Options) -> Option<Paired> {
        let per = |ns: u64| ns as f64 / iterations as f64;
        let mut kept: Vec<(f64, f64)> = pairs.iter().map(|&(b, c)| (per(b), per(c))).collect();
        if options.filter_outliers
            && let Some((low, high)) = fences(kept.iter().map(|(b, c)| c - b).collect())
        {
            kept.retain(|(b, c)| (low..=high).contains(&(c - b)));
        }
        let diff = Summary::of(&kept.iter().map(|(b, c)| c - b).collect::<Vec<_>>())?;
        let baseline = Summary::of(&kept.iter().map(|it| it.0).collect::<Vec<_>>())?;
        let candidate = Summary::of(&kept.iter().map(|it| it.1).collect::<Vec<_>>())?;
        let z = diff.mean / (diff.variance.sqrt() / (diff.n as f64).sqrt());
        let percent = diff.mean / baseline.mean * 100.0;
        let significant = z.abs() >= Z_SIGNIFICANT && percent.abs() > options.noise_threshold;
        Some(Paired {
            baseline_mean: baseline.mean,
            candidate_mean: candidate.mean,
            percent,
            significant,
        })
    }
}

/// Tukey's fences of `values`, 1.5 interquartile ranges (at least 1 ns) past the quartiles,
/// `None` for fewer than four values.
fn fences(mut values: Vec<f64>) -> Option<(f64, f64)> {
    values.sort_unstable_by(f64::total_cmp);
    let (q1, q3) = (values.len() / 4, (values.len() * 3 / 4).checked_sub(1)?);
    if q1 >= q3 {
        return None;
    }
    let range = (values[q3] - values[q1]).max(1.0);
    Some((values[q1] - 1.5 * range, values[q3] + 1.5 * range))
}

/// Nanoseconds, in the largest unit below them.
struct Human(f64);

impl fmt::Display for Human {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ns = self.0;
        let shown = match ns.abs() {
            it if it > 1e9 => format!("{:.1} s", ns / 1e9),
            it if it > 1e6 => format!("{:.1} ms", ns / 1e6),
            it if it > 1e3 => format!("{:.1} us", ns / 1e3),
            _ if ns == 0.0 => "0 ns".to_owned(),
            _ => format!("{ns:.1} ns"),
        };
        f.pad(&shown)
    }
}

#[cfg(test)]
mod tests;
