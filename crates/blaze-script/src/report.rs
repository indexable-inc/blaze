//! The one shape of a harness's result: `harness/v2/report.py` ported whole.
//!
//! A script makes a [`Report`], adds an [`Outcome`] for each thing it checked, and ends with
//! [`Report::finish`], which writes the result to the script's `out` (JSON) and exits:
//!
//! ```text
//! pass        what was checked holds
//! fail        it does not: a finding about the compiler
//! unmeasured  the engine has no answer to it (`Fault::Unmeasurable`): a failure
//! error       the harness could not check it (`Fault::Failed`)
//! ```
//!
//! The exit status is 1 when anything failed, erred or was unmeasured; else 0. The file is the
//! Python's byte for byte but for the times and the host (`json.dumps(result, indent=1)`):
//!
//! ```text
//! {"script": "oracle.py", "engine": {...}, "host": {...}, "commit": "...", "started": "...",
//!  "ended": "...", "outcomes": [{"name", "verdict", "detail", "numbers"}], "counts": {...},
//!  "extra": {...}}
//! ```

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::{
    engine::{EngineSpec, Fault, root},
    pyfmt::float_repr,
    pyjson::dumps_indent,
    pypath::pure_path,
    pystr,
};

/// What became of one thing checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Pass,
    Fail,
    Unmeasured,
    Error,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Unmeasured => "unmeasured",
            Verdict::Error => "error",
        }
    }
}

/// One of an outcome's numbers: `int | float | str | bool | None`.
#[derive(Debug, Clone, PartialEq)]
pub enum Number {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    None,
}

impl Number {
    /// `str(v)`, as an f-string writes it.
    pub fn shown(&self) -> String {
        match self {
            Number::Int(n) => n.to_string(),
            Number::Float(f) => float_repr(*f),
            Number::Str(s) => s.clone(),
            Number::Bool(true) => "True".to_owned(),
            Number::Bool(false) => "False".to_owned(),
            Number::None => "None".to_owned(),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Number::Int(n) => Value::from(*n),
            Number::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Number::Str(s) => Value::String(s.clone()),
            Number::Bool(b) => Value::Bool(*b),
            Number::None => Value::Null,
        }
    }
}

impl From<i64> for Number {
    fn from(n: i64) -> Number {
        Number::Int(n)
    }
}

impl From<usize> for Number {
    fn from(n: usize) -> Number {
        Number::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }
}

impl From<u64> for Number {
    fn from(n: u64) -> Number {
        Number::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Number {
    fn from(f: f64) -> Number {
        Number::Float(f)
    }
}

impl From<&str> for Number {
    fn from(s: &str) -> Number {
        Number::Str(s.to_owned())
    }
}

impl From<bool> for Number {
    fn from(b: bool) -> Number {
        Number::Bool(b)
    }
}

/// One thing checked, with its numbers in the order they were given.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
    pub numbers: Vec<(String, Number)>,
}

impl Outcome {
    pub fn new(name: &str, verdict: Verdict, detail: &str) -> Outcome {
        Outcome { name: name.to_owned(), verdict, detail: detail.to_owned(), numbers: Vec::new() }
    }

    /// `dataclasses.asdict(outcome)`.
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".to_owned(), Value::String(self.name.clone()));
        out.insert("verdict".to_owned(), Value::String(self.verdict.name().to_owned()));
        out.insert("detail".to_owned(), Value::String(self.detail.clone()));
        let numbers: Map<String, Value> = self.numbers.iter().map(|(k, v)| (k.clone(), v.to_value())).collect();
        out.insert("numbers".to_owned(), Value::Object(numbers));
        Value::Object(out)
    }

    /// The line `Report.add` prints: `PASS       name  k=v  first line of the detail`.
    pub fn line(&self) -> String {
        let shown: Vec<String> = self.numbers.iter().map(|(k, v)| format!("{k}={}", v.shown())).collect();
        let first = pystr::splitlines(&self.detail).first().map(|it| crate::engine::first_chars(it, 220).to_owned()).unwrap_or_default();
        let verdict = self.verdict.name().to_uppercase();
        pystr::rstrip(&format!("{verdict:10} {}  {}  {first}", self.name, shown.join("  "))).to_owned()
    }
}

/// The last commit of the checkout, or None.
pub fn commit() -> Option<String> {
    let root = root();
    let ran = crate::run(&["git", "-C", &root.display().to_string(), "rev-parse", "HEAD"], Path::new(".")).ok()?;
    let said = pystr::strip(&ran.out()).to_owned();
    (!said.is_empty()).then_some(said)
}

#[cfg(target_os = "linux")]
const SC_NPROCESSORS_ONLN: std::ffi::c_int = 84;
#[cfg(target_os = "macos")]
const SC_NPROCESSORS_ONLN: std::ffi::c_int = 58;

/// `os.cpu_count()`: `sysconf(_SC_NPROCESSORS_ONLN)`, as Python 3.13 asks it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cpu_count() -> Option<i64> {
    unsafe extern "C" {
        fn sysconf(name: std::ffi::c_int) -> std::ffi::c_long;
    }
    // SAFETY: sysconf takes a constant and reads no memory of the caller's.
    let n = unsafe { sysconf(SC_NPROCESSORS_ONLN) };
    (n >= 1).then_some(i64::from(n))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cpu_count() -> Option<i64> {
    None
}

/// `len(os.sched_getaffinity(0))` on Linux; `os.cpu_count()` elsewhere.
#[cfg(target_os = "linux")]
fn cores() -> Option<i64> {
    unsafe extern "C" {
        fn sched_getaffinity(pid: std::ffi::c_int, size: usize, mask: *mut u8) -> std::ffi::c_int;
    }
    let mut mask = [0u8; 128];
    // SAFETY: the mask is 128 bytes, the size given; pid 0 is this process.
    if unsafe { sched_getaffinity(0, mask.len(), mask.as_mut_ptr()) } != 0 {
        return cpu_count();
    }
    Some(mask.iter().map(|b| i64::from(b.count_ones())).sum())
}

#[cfg(not(target_os = "linux"))]
fn cores() -> Option<i64> {
    cpu_count()
}

fn uname(flag: &str) -> String {
    crate::run(&["uname", flag], Path::new("/")).map(|it| pystr::strip(&it.out()).to_owned()).unwrap_or_default()
}

fn memory_gib() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        let kb = text.lines().find(|line| line.starts_with("MemTotal:")).and_then(|line| line.split_whitespace().nth(1));
        if let Some(Ok(kb)) = kb.map(str::parse::<u64>) {
            return Some(crate::py_round(kb as f64 / f64::from(1u32 << 20), 1));
        }
    }
    if cfg!(target_os = "macos") {
        let out = crate::run(&["sysctl", "-n", "hw.memsize"], Path::new("/")).map(|it| pystr::strip(&it.out()).to_owned()).ok()?;
        if !out.is_empty() && out.bytes().all(|b| b.is_ascii_digit()) {
            return Some(crate::py_round(out.parse::<f64>().ok()? / f64::from(1u32 << 30), 1));
        }
    }
    None
}

/// What the run was on.
pub fn host() -> Value {
    let opt = |n: Option<i64>| n.map_or(Value::Null, Value::from);
    let mut out = Map::new();
    out.insert("os".to_owned(), Value::String(uname("-s")));
    out.insert("arch".to_owned(), Value::String(uname("-m")));
    out.insert("kernel".to_owned(), Value::String(uname("-r")));
    out.insert("cores".to_owned(), opt(cores()));
    out.insert("machine_cores".to_owned(), opt(cpu_count()));
    out.insert("memory_gib".to_owned(), memory_gib().map_or(Value::Null, |f| Number::Float(f).to_value()));
    #[allow(clippy::disallowed_methods, reason = "report.py's field, read where it is used as the Python read it")]
    let name = std::env::var("BLAZE_BENCH_MACHINE").ok();
    out.insert("name".to_owned(), name.map_or(Value::Null, Value::String));
    Value::Object(out)
}

/// A run's result, built one outcome at a time.
#[derive(Debug)]
pub struct Report {
    pub script: String,
    pub engine: EngineSpec,
    pub out: Option<PathBuf>,
    pub started: String,
    pub outcomes: Vec<Outcome>,
    pub extra: Map<String, Value>,
}

impl Report {
    pub fn new(script: &str, engine: &EngineSpec, out: &str) -> Report {
        Report {
            script: script.to_owned(),
            engine: engine.clone(),
            out: (!out.is_empty()).then(|| PathBuf::from(pure_path(out))),
            started: crate::utc_now(),
            outcomes: Vec::new(),
            extra: Map::new(),
        }
    }

    /// The outcome of `name`, printed as it is added; `numbers` in the order given.
    pub fn add(&mut self, name: &str, verdict: Verdict, detail: &str, numbers: Vec<(&str, Number)>) -> Outcome {
        let outcome = Outcome {
            name: name.to_owned(),
            verdict,
            detail: detail.to_owned(),
            numbers: numbers.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        };
        self.add_outcome(outcome.clone());
        outcome
    }

    /// An outcome made elsewhere (on a thread of a fan-out), added and printed.
    pub fn add_outcome(&mut self, outcome: Outcome) {
        println!("{}", outcome.line());
        self.outcomes.push(outcome);
    }

    /// Python's `guarded(name, fn)` is `guard(name, fn())`: the value, or None with an outcome
    /// for `name` when it could not be done (unmeasured, or error). An `OSError` the Python let
    /// through ends the script, naming it.
    pub fn guard<T>(&mut self, name: &str, result: Result<T, Fault>) -> Option<T> {
        match result {
            Ok(it) => Some(it),
            Err(Fault::Unmeasurable(said)) => {
                self.add(name, Verdict::Unmeasured, &said, Vec::new());
                None
            }
            Err(Fault::Failed(said)) => {
                self.add(name, Verdict::Error, &said, Vec::new());
                None
            }
            Err(Fault::Os(said)) => crate::fail(&self.script, &said),
        }
    }

    pub fn failing(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| matches!(o.verdict, Verdict::Fail | Verdict::Error | Verdict::Unmeasured)).collect()
    }

    /// The counts by verdict, in the order each was first seen (`Counter`'s).
    pub fn counts(&self) -> Vec<(Verdict, usize)> {
        let mut counts: Vec<(Verdict, usize)> = Vec::new();
        for o in &self.outcomes {
            match counts.iter_mut().find(|(v, _)| *v == o.verdict) {
                Some((_, n)) => *n += 1,
                None => counts.push((o.verdict, 1)),
            }
        }
        counts
    }

    /// The result as written, with `host` and `commit` given.
    pub fn result(&self, host: Value, commit: Option<String>, ended: &str) -> Value {
        let mut result = Map::new();
        result.insert("script".to_owned(), Value::String(self.script.clone()));
        result.insert("engine".to_owned(), self.engine.to_value());
        result.insert("host".to_owned(), host);
        result.insert("commit".to_owned(), commit.map_or(Value::Null, Value::String));
        result.insert("started".to_owned(), Value::String(self.started.clone()));
        result.insert("ended".to_owned(), Value::String(ended.to_owned()));
        result.insert("outcomes".to_owned(), Value::Array(self.outcomes.iter().map(Outcome::to_value).collect()));
        let counts: Map<String, Value> = self.counts().into_iter().map(|(v, n)| (v.name().to_owned(), Value::from(n))).collect();
        result.insert("counts".to_owned(), Value::Object(counts));
        result.insert("extra".to_owned(), Value::Object(self.extra.clone()));
        Value::Object(result)
    }

    /// The closing line: `<script>: 1 error, 3 pass; result <out>`.
    pub fn summary(&self) -> String {
        let mut counts = self.counts();
        counts.sort_by_key(|(v, _)| v.name());
        let listed: Vec<String> = counts.iter().map(|(v, n)| format!("{n} {}", v.name())).collect();
        let result = self.out.as_ref().map(|out| format!("; result {}", out.display())).unwrap_or_default();
        format!("{}: {}{result}", self.script, listed.join(", "))
    }

    /// Writes the result to `out`, prints the closing line and exits: 1 when anything failed,
    /// erred or was unmeasured.
    pub fn finish(self) -> ! {
        let result = self.result(host(), commit(), &crate::utc_now());
        if let Some(out) = &self.out {
            if let Some(parent) = out.parent().filter(|it| !it.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    crate::fail(&self.script, &crate::os_error(&e, &parent.display().to_string()));
                }
            }
            if let Err(e) = std::fs::write(out, dumps_indent(&result, 1) + "\n") {
                crate::fail(&self.script, &crate::os_error(&e, &out.display().to_string()));
            }
        }
        println!("{}", self.summary());
        crate::exit(if self.failing().is_empty() { 0 } else { 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EngineSpec {
        EngineSpec { kind: "v2".to_owned(), blaze: "b".to_owned(), backend: "cranelift".to_owned(), dev_backend: "default".to_owned() }
    }

    #[test]
    fn an_outcome_prints_as_report_py_prints_it() {
        let mut rep = Report::new("share.py", &spec(), "/tmp/x/../r.json");
        let o = rep.add("share/a", Verdict::Pass, "", vec![("blaze_s", Number::Float(1.5)), ("n", Number::Int(2))]);
        assert_eq!(o.line(), "PASS       share/a  blaze_s=1.5  n=2");
        let o = rep.add("b", Verdict::Unmeasured, &format!("{}\nsecond", "x".repeat(300)), vec![]);
        assert_eq!(o.line(), format!("UNMEASURED b    {}", "x".repeat(220)));
        let o = rep.add("c", Verdict::Fail, "why", vec![("ok", Number::Bool(true)), ("none", Number::None)]);
        assert_eq!(o.line(), "FAIL       c  ok=True  none=None  why");
        rep.add("d", Verdict::Pass, "", vec![]);
        assert_eq!(rep.summary(), "share.py: 1 fail, 2 pass, 1 unmeasured; result /tmp/x/../r.json");
        assert_eq!(rep.failing().len(), 2);
    }

    #[test]
    fn the_result_is_report_pys_json() {
        let mut rep = Report::new("s.py", &spec(), "");
        rep.started = "2026-11-11T00:00:00Z".to_owned();
        rep.add("a", Verdict::Error, "d", vec![("x", Number::Float(2.0))]);
        rep.add("b", Verdict::Pass, "", vec![]);
        rep.extra.insert("work".to_owned(), Value::String("/w".to_owned()));
        let said = dumps_indent(&rep.result(Value::Object(Map::new()), None, "2026-11-11T00:00:01Z"), 1);
        // json.dumps(result, indent=1) of report.py's dict, by Python 3.13.
        let want = "{\n \"script\": \"s.py\",\n \"engine\": {\n  \"kind\": \"v2\",\n  \"blaze\": \"b\",\n  \"backend\": \"cranelift\",\n  \"dev_backend\": \"default\"\n },\n \"host\": {},\n \"commit\": null,\n \"started\": \"2026-11-11T00:00:00Z\",\n \"ended\": \"2026-11-11T00:00:01Z\",\n \"outcomes\": [\n  {\n   \"name\": \"a\",\n   \"verdict\": \"error\",\n   \"detail\": \"d\",\n   \"numbers\": {\n    \"x\": 2.0\n   }\n  },\n  {\n   \"name\": \"b\",\n   \"verdict\": \"pass\",\n   \"detail\": \"\",\n   \"numbers\": {}\n  }\n ],\n \"counts\": {\n  \"error\": 1,\n  \"pass\": 1\n },\n \"extra\": {\n  \"work\": \"/w\"\n }\n}";
        assert_eq!(said, want);
        assert!(rep.out.is_none() && rep.summary() == "s.py: 1 error, 1 pass");
    }

    #[test]
    fn a_guarded_step_is_an_outcome() {
        let mut rep = Report::new("s.py", &spec(), "");
        assert_eq!(rep.guard("a", Ok(3)), Some(3));
        assert_eq!(rep.guard::<()>("b", Err(Fault::Failed("no".to_owned()))), None);
        assert_eq!(rep.guard::<()>("c", Err(Fault::Unmeasurable("none".to_owned()))), None);
        let verdicts: Vec<Verdict> = rep.outcomes.iter().map(|o| o.verdict).collect();
        assert_eq!(verdicts, vec![Verdict::Error, Verdict::Unmeasured]);
    }
}
