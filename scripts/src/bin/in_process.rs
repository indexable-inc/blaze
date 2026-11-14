//! in_process: what serving a command in its own process costs (`[serve] host = "process"`,
//! `blaze_thin::host`, `serve::once`) next to a daemon (`host = "daemon"`), on one real project:
//! step 2 of the architecture audit of 2026-11-13 (verdict (b), no long-lived daemon as the home
//! of state). `DOC` below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin in_process -- '{"project": "/w/reth", "blaze": "/b/blaze", "timeout_s": 1800,
//!         "edit": {"file": "crates/x/src/lib.rs", "find": "x + 1", "replace": ["x + 2"]}}'
//!     blaze run -q -p scripts --bin in_process -- selftest
//!
//! Each repetition gives each host a fresh `BLAZE_HOME` (so a fresh build directory, store and
//! image: `blaze_manifest::home::build_dir` is under it), with its `config.toml` naming the host,
//! and runs the edit loop's five operations in order. Each operation is one child, its wall
//! time measured around its spawn and its reaping, and its peak resident memory the kernel's
//! (`wait4`'s `ru_maxrss`, the child alone: a daemon is not the client's child it waits for, so
//! a daemon's memory is what `blaze status --json` says of it after the operation). The hosts
//! alternate within a repetition, so drift of the machine falls on both.
//!
//! It names what it measured (rule measure-the-exact-tree): the blaze checkout's commit, its
//! uncommitted diff's hash and untracked files, the hash of the binary and of the engine beside
//! it, and the project's commit.

/// What the usage error prints.
const DOC: &str = r#"in_process: the edit loop under each serve host, side by side (`[serve] host` in BLAZE_HOME/config.toml).

    blaze run -q -p scripts --bin in_process -- '<json>'
    blaze run -q -p scripts --bin in_process -- selftest

    project     the project's directory (its workspace root), required
    blaze       the blaze binary measured (the thin `blaze`; `blaze-engine` beside it is hashed too), required
    timeout_s   each run's bound in seconds; a run past it is killed and the script fails, required
    edit        the edits, required:
        file      the file edited, from the project
        find      text that occurs exactly once in it, the body edit's slot
        replace   one or more texts, none in the file: the k-th body edit puts the k-th of
                  [find, replace...] (cycling) where the slot's text is now
        comment   how a line comment begins ("//")
    command     "build" or "check" ("build")
    args        more arguments to each run (["-p", "x"]); none by default
    reps        repetitions, each on fresh homes (3)
    hosts       which hosts, in order (["daemon", "process"])
    floor_ms    the floor of each operation in ms, by its name ({}), for the ratio; one not named has none
    out         the JSON file written ("in_process.json")
    tree        the blaze checkout the binary was built from (".")
    work        where the homes, temporary directories and logs go (a new temporary directory)
    keep_homes  keep each repetition's homes (false: each is removed once measured)

The operations, in order, each one run of `blaze <command> <args>` in the project:
    cold          on a fresh home
    noop          nothing changed
    comment       a comment line appended to the file
    body          the slot's text replaced
    restart_body  the daemon stopped (`blaze stop`; nothing is resident in process mode
                  anyway), then the slot's text replaced again

Per operation and host: wall ms, the client's peak RSS (the kernel's, of the child), the
daemon's memory after it (`blaze status --json`; daemon host only), and bytes on disk of the
home (allocated blocks, each inode once) after it; over the reps the median and the range, and
the median's ratio to the floor. The file is restored after every repetition and on failure; a
failed run (status not 0) stops the script, naming the operation and its log.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_script::{
    TempDir,
    child::{self, Ended, Measured},
    expect, fail, git,
    input::{self, Fields, Input, InputError},
    run,
};
use serde_json::{Value, json};

const SCRIPT: &str = "in_process";
/// The operations, in the order each repetition runs them.
const OPS: [&str; 5] = ["cold", "noop", "comment", "body", "restart_body"];
/// The hosts, as `[serve] host` names them (`blaze_thin::host::Host::word`).
const HOSTS: [&str; 2] = ["daemon", "process"];

struct Edit {
    file: String,
    find: String,
    replace: Vec<String>,
    comment: String,
}

impl Input for Edit {
    const FIELDS: &'static [&'static str] = &["file", "find", "replace", "comment"];

    fn parse(f: &Fields<'_>) -> Result<Edit, InputError> {
        let edit = Edit {
            file: f.str("file", None)?,
            find: f.str("find", None)?,
            replace: f.strs_required("replace")?,
            comment: f.str("comment", Some("//"))?,
        };
        let slot = edit.slot();
        let distinct: BTreeSet<&String> = slot.iter().collect();
        if edit.replace.is_empty() || distinct.len() != slot.len() || slot.iter().any(String::is_empty) {
            return Err(InputError(format!("{}: find and every replace are texts, none empty and no two alike", f.at("replace"))));
        }
        Ok(edit)
    }
}

impl Edit {
    /// What the slot holds in turn: `find`, then each `replace`, cycling.
    fn slot(&self) -> Vec<String> {
        let mut all = vec![self.find.clone()];
        all.extend(self.replace.iter().cloned());
        all
    }
}

struct Args {
    project: String,
    blaze: String,
    timeout_s: i64,
    edit: Edit,
    command: String,
    args: Vec<String>,
    reps: i64,
    hosts: Vec<String>,
    floor_ms: BTreeMap<String, f64>,
    out: String,
    tree: String,
    work: Option<String>,
    keep_homes: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] =
        &["project", "blaze", "timeout_s", "edit", "command", "args", "reps", "hosts", "floor_ms", "out", "tree", "work", "keep_homes"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let timeout_s = f.int("timeout_s", None)?;
        let reps = f.int("reps", Some(3))?;
        if timeout_s < 1 || reps < 1 {
            return Err(InputError(format!("{} and {} are at least 1", f.at("timeout_s"), f.at("reps"))));
        }
        let hosts = f.ones_of("hosts", &HOSTS, &HOSTS)?;
        if hosts.is_empty() || hosts.iter().collect::<BTreeSet<_>>().len() != hosts.len() {
            return Err(InputError(format!("{} names each host at most once, and one at least", f.at("hosts"))));
        }
        Ok(Args {
            project: f.str("project", None)?,
            blaze: f.str("blaze", None)?,
            timeout_s,
            edit: f.object_required("edit")?,
            command: f.one_of("command", &["build", "check"], Some("build"))?,
            args: f.strs("args")?,
            reps,
            hosts,
            floor_ms: floors(&f.any("floor_ms"), &f.at("floor_ms"))?,
            out: f.str("out", Some("in_process.json"))?,
            tree: f.str("tree", Some("."))?,
            work: f.str_or_null("work")?,
            keep_homes: f.bool("keep_homes", Some(false))?,
        })
    }
}

/// The floors by operation: an object of numbers keyed by names of [`OPS`], or null.
fn floors(value: &Value, at: &str) -> Result<BTreeMap<String, f64>, InputError> {
    let Value::Object(given) = value else {
        return match value {
            Value::Null => Ok(BTreeMap::new()),
            other => Err(InputError(format!("{at} is an object of ms by operation, not {other}"))),
        };
    };
    given
        .iter()
        .map(|(op, ms)| match (OPS.contains(&op.as_str()), ms.as_f64()) {
            (true, Some(ms)) if ms > 0.0 => Ok((op.clone(), ms)),
            (true, _) => Err(InputError(format!("{at}.{op} is a number of ms above 0, not {ms}"))),
            (false, _) => Err(InputError(format!("{at}.{op}: no such operation (they are {})", OPS.join(", ")))),
        })
        .collect()
}

/// Runs `argv` in `cwd` with exactly `env`, its outputs to `<log>.out` and `<log>.err`, the
/// kernel's peak RSS of it coming back ([`child::run_logged`]). Bounded by `limit`: past it the
/// child's process group is killed and this is `Err`.
fn measure(argv: &[String], cwd: &Path, env: &[(&str, &Path)], log: &Path, limit: Duration) -> Result<Measured, String> {
    match child::run_logged(argv, cwd, env, log, limit)? {
        Ended::Exited(it) => Ok(it),
        Ended::TimedOut { after } => Err(format!("killed at its bound of {} s (log {})", after.as_secs(), log.display())),
    }
}

/// Bytes on disk under `root`: allocated blocks, each inode once, links not followed; 0 when
/// it is not there.
fn disk_bytes(root: &Path) -> u64 {
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let mut todo = vec![root.to_path_buf()];
    // Bounded: each directory is listed once, and links are not followed.
    while let Some(at) = todo.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&at) else { continue };
        if seen.insert((meta.dev(), meta.ino())) {
            total += meta.blocks() * 512;
        }
        if meta.is_dir() {
            let Ok(listed) = std::fs::read_dir(&at) else { continue };
            todo.extend(listed.filter_map(Result::ok).map(|entry| entry.path()));
        }
    }
    total
}

/// The run: what every repetition shares.
struct Bench<'a> {
    it: &'a Args,
    project: PathBuf,
    blaze: PathBuf,
    file: PathBuf,
    original: String,
    work: PathBuf,
    home_dir: PathBuf,
    path: std::ffi::OsString,
    limit: Duration,
    /// The homes whose daemon may be running, stopped before the script ends either way.
    daemons: Vec<PathBuf>,
}

impl Bench<'_> {
    /// Ends the script with `message`, the file restored and every daemon it started stopped.
    fn die(&self, message: &str) -> ! {
        let _ = std::fs::write(&self.file, &self.original);
        for home in &self.daemons {
            let _ = self.stop(home);
        }
        fail(SCRIPT, message)
    }

    fn env<'b>(&'b self, home: &'b Path, tmp: &'b Path) -> Vec<(&'b str, &'b Path)> {
        vec![("HOME", self.home_dir.as_path()), ("PATH", Path::new(&self.path)), ("BLAZE_HOME", home), ("TMPDIR", tmp)]
    }

    /// `blaze <args>` in the project under `home`, unmeasured but bounded, its output kept.
    fn blaze(&self, home: &Path, args: &[&str]) -> Result<String, String> {
        let tmp = home.with_extension("tmp");
        let log = home.with_extension(args.first().copied().unwrap_or("blaze"));
        let mut argv = vec![self.blaze.display().to_string()];
        argv.extend(args.iter().map(|it| (*it).to_owned()));
        let ran = measure(&argv, &self.project, &self.env(home, &tmp), &log, self.limit)?;
        let out = std::fs::read_to_string(log.with_extension("out")).unwrap_or_default();
        match ran.status {
            0 => Ok(out),
            status => Err(format!("blaze {} exited {status} (log {})", args.join(" "), log.display())),
        }
    }

    /// The daemon of `home`, as `blaze status --json` says: its pid and memory, `None` with none.
    fn daemon(&self, home: &Path) -> Option<(i32, u64)> {
        let out = self.blaze(home, &["status", "--json"]).ok()?;
        let value: Value = serde_json::from_str(out.trim()).ok()?;
        let daemon = value.get("daemon")?;
        Some((i32::try_from(daemon.get("pid")?.as_u64()?).ok()?, daemon.get("memory_bytes").and_then(Value::as_u64).unwrap_or(0)))
    }

    /// `blaze stop` for `home`, and the daemon's process gone, within the bound.
    fn stop(&self, home: &Path) -> Result<(), String> {
        let Some((pid, _)) = self.daemon(home) else { return Ok(()) };
        self.blaze(home, &["stop"])?;
        let deadline = Instant::now() + self.limit;
        // Bounded: the run's bound.
        while child::alive(pid) {
            if Instant::now() >= deadline {
                return Err(format!("the daemon of {} (pid {pid}) outlived `blaze stop` by {} s", home.display(), self.limit.as_secs()));
            }
            #[allow(clippy::disallowed_methods, reason = "a script waits for a process to go; it is on no pool")]
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// One repetition on `host`: a fresh home, the five operations, the home measured after
    /// each; the file restored at the end.
    fn rep(&mut self, host: &str, rep: i64) -> Vec<Value> {
        let home = self.work.join(format!("home-{host}-{rep}"));
        let tmp = home.with_extension("tmp");
        for dir in [&home, &tmp] {
            let _ = std::fs::remove_dir_all(dir);
            if let Err(e) = std::fs::create_dir_all(dir) {
                self.die(&format!("{} could not be made: {e}", dir.display()));
            }
        }
        if let Err(e) = std::fs::write(home.join("config.toml"), format!("[serve]\nhost = \"{host}\"\n")) {
            self.die(&format!("{}/config.toml could not be written: {e}", home.display()));
        }
        if host == "daemon" {
            self.daemons.push(home.clone());
        }
        let slot = self.it.edit.slot();
        let mut at = 0usize;
        let mut argv = vec![self.blaze.display().to_string(), self.it.command.clone()];
        argv.extend(self.it.args.iter().cloned());
        let mut rows = Vec::new();
        for op in OPS {
            let edited = match op {
                "comment" => {
                    let text = std::fs::read_to_string(&self.file).unwrap_or_default();
                    let line = format!("{} in_process: {host}, repetition {rep}\n", self.it.edit.comment);
                    let sep = if text.ends_with('\n') || text.is_empty() { "" } else { "\n" };
                    std::fs::write(&self.file, format!("{text}{sep}{line}")).map_err(|e| e.to_string())
                }
                "body" | "restart_body" => {
                    if op == "restart_body" && host == "daemon" {
                        if let Err(why) = self.stop(&home) {
                            self.die(&format!("{host} repetition {rep}, before {op}: {why}"));
                        }
                    }
                    let (from, to) = (&slot[at % slot.len()], &slot[(at + 1) % slot.len()]);
                    at += 1;
                    replace_once(&self.file, from, to)
                }
                _ => Ok(()),
            };
            if let Err(why) = edited {
                self.die(&format!("{host} repetition {rep}, {op}: the edit of {}: {why}", self.file.display()));
            }
            let log = self.work.join(format!("{host}-{rep}-{op}"));
            let ran = measure(&argv, &self.project, &self.env(&home, &tmp), &log, self.limit)
                .unwrap_or_else(|why| self.die(&format!("{host} repetition {rep}, {op}: {why}")));
            if ran.status != 0 {
                self.die(&format!("{host} repetition {rep}, {op}: blaze exited {} (log {})", ran.status, log.with_extension("err").display()));
            }
            let daemon = if host == "daemon" { self.daemon(&home) } else { None };
            rows.push(json!({
                "host": host, "rep": rep, "op": op,
                "wall_ns": ran.wall_ns, "cpu_ns": ran.cpu_ns, "client_peak_rss_bytes": ran.rss_bytes,
                "daemon_pid": daemon.map(|it| it.0), "daemon_memory_bytes": daemon.map(|it| it.1),
                "home_disk_bytes": disk_bytes(&home), "log": log.display().to_string(),
            }));
        }
        if host == "daemon" {
            if let Err(why) = self.stop(&home) {
                self.die(&format!("{host} repetition {rep}, the stop at its end: {why}"));
            }
        }
        self.daemons.retain(|it| it != &home);
        let at_end = disk_bytes(&home);
        for row in &mut rows {
            row["home_disk_bytes_at_end"] = json!(at_end);
        }
        if let Err(e) = std::fs::write(&self.file, &self.original) {
            self.die(&format!("{} could not be restored: {e}", self.file.display()));
        }
        if !self.it.keep_homes {
            let _ = std::fs::remove_dir_all(&home);
            let _ = std::fs::remove_dir_all(&tmp);
        }
        rows
    }
}

/// `from` replaced with `to` in `file`, `from` occurring there exactly once.
fn replace_once(file: &Path, from: &str, to: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let count = text.matches(from).count();
    if count != 1 {
        return Err(format!("{from:?} occurs {count} times, not once"));
    }
    std::fs::write(file, text.replacen(from, to, 1)).map_err(|e| e.to_string())
}

/// The median, least and most of `values`; `None` when there are none.
fn spread(values: &[f64]) -> Option<(f64, f64, f64)> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = match n {
        0 => return None,
        _ if n % 2 == 1 => sorted[n / 2],
        _ => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    };
    Some((median, sorted[0], sorted[n - 1]))
}

/// The git blob hash of the file at `path` (`git hash-object`), `None` when there is none.
fn hash_of(tree: &Path, path: &Path) -> Option<String> {
    path.is_file().then(|| git(tree, &["hash-object", "--no-filters", &path.display().to_string()]).ok()).flatten().map(|it| it.trim().to_owned())
}

/// The exact tree: the checkout's commit, its diff's hash and untracked files.
fn tree_of(tree: &Path, work: &Path) -> Result<Value, String> {
    let head = git(tree, &["rev-parse", "HEAD"])?.trim().to_owned();
    let status = git(tree, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let diff = run(&["git", "diff", "HEAD", "--binary"], tree)?;
    let diff_path = work.join("tree.diff");
    std::fs::write(&diff_path, &diff.stdout).map_err(|e| format!("{}: {e}", diff_path.display()))?;
    let untracked: Vec<&str> = status.lines().filter_map(|it| it.strip_prefix("?? ")).collect();
    Ok(json!({
        "root": tree.display().to_string(), "commit": head, "dirty": !status.trim().is_empty(),
        "diff_hash": (!diff.stdout.is_empty()).then(|| hash_of(tree, &diff_path)).flatten(), "untracked": untracked,
    }))
}

fn main_with(it: &Args) {
    let canon = |what: &str, path: &str| std::fs::canonicalize(path).unwrap_or_else(|e| fail(SCRIPT, &format!("the {what} {path}: {e}")));
    let (project, blaze, tree) = (canon("project", &it.project), canon("blaze", &it.blaze), canon("tree", &it.tree));
    let temp = match &it.work {
        Some(_) => None,
        None => Some(TempDir::new("blaze-in-process-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()))),
    };
    let work = match (&it.work, &temp) {
        (Some(work), _) => PathBuf::from(work),
        (None, Some(temp)) => temp.path().to_path_buf(),
        (None, None) => unreachable!("a temporary directory is made when no work directory is given"),
    };
    std::fs::create_dir_all(&work).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", work.display())));
    let file = project.join(&it.edit.file);
    let original = std::fs::read_to_string(&file).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", file.display())));
    if original.matches(&it.edit.find).count() != 1 || it.edit.replace.iter().any(|r| original.contains(r.as_str())) {
        fail(SCRIPT, &format!("{}: edit.find must occur there exactly once and no edit.replace at all", file.display()));
    }
    // The two variables a measured blaze inherits, as compiletest's runs do: `HOME` (the
    // toolchain blaze fetched is the machine's, not a fresh home's) and `PATH` (the C driver).
    #[allow(clippy::disallowed_methods, reason = "a script hands the caller's HOME and PATH to the program it measures, and nothing else")]
    let (home_dir, path) = (std::env::var_os("HOME"), std::env::var_os("PATH"));
    let (Some(home_dir), Some(path)) = (home_dir.map(PathBuf::from), path) else {
        fail(SCRIPT, "HOME and PATH are set: the measured blaze inherits both and nothing else")
    };
    let named = json!({
        "tree": tree_of(&tree, &work).unwrap_or_else(|why| fail(SCRIPT, &why)),
        "binary": {"path": blaze.display().to_string(), "hash": hash_of(&tree, &blaze),
                   "engine_hash": hash_of(&tree, &blaze.with_file_name("blaze-engine"))},
        "project": {"root": project.display().to_string(),
                    "commit": git(&project, &["rev-parse", "HEAD"]).ok().map(|it| it.trim().to_owned())},
        "host_os": std::env::consts::OS, "started": blaze_script::utc_now(),
    });
    let mut bench = Bench {
        it,
        project,
        blaze,
        file,
        original,
        work: work.clone(),
        home_dir,
        path,
        limit: Duration::from_secs(u64::try_from(it.timeout_s).unwrap_or(1)),
        daemons: Vec::new(),
    };
    let mut runs = Vec::new();
    for rep in 0..it.reps {
        for host in &it.hosts {
            eprintln!("{SCRIPT}: {host}, repetition {} of {}", rep + 1, it.reps);
            runs.extend(bench.rep(host, rep));
        }
    }
    let summary = summarize(it, &runs);
    print_table(it, &summary, &named);
    let out = json!({"named": named, "input": {"command": it.command, "args": it.args, "reps": it.reps, "timeout_s": it.timeout_s,
                     "file": it.edit.file, "floor_ms": it.floor_ms}, "summary": summary, "runs": runs});
    let text = serde_json::to_string_pretty(&out).unwrap_or_default() + "\n";
    if let Err(e) = std::fs::write(&it.out, text) {
        fail(SCRIPT, &format!("{} could not be written: {e}", it.out));
    }
    println!("{SCRIPT}: wrote {}", it.out);
}

/// Per host and operation: the median and range of each measure, and the wall's ratio to its floor.
fn summarize(it: &Args, runs: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for host in &it.hosts {
        for op in OPS {
            let these: Vec<&Value> = runs.iter().filter(|r| r["host"] == host.as_str() && r["op"] == op).collect();
            let of = |field: &str| -> Value {
                let values: Vec<f64> = these.iter().filter_map(|r| r[field].as_f64()).collect();
                match spread(&values) {
                    Some((median, min, max)) => json!({"median": median, "min": min, "max": max}),
                    None => Value::Null,
                }
            };
            let wall = of("wall_ns");
            let floor = it.floor_ms.get(op).copied();
            let ratio = match (wall["median"].as_f64(), floor) {
                (Some(ns), Some(ms)) => json!(ns / 1e6 / ms),
                _ => Value::Null,
            };
            out.push(json!({
                "host": host, "op": op, "wall_ns": wall, "cpu_ns": of("cpu_ns"),
                "client_peak_rss_bytes": of("client_peak_rss_bytes"), "daemon_memory_bytes": of("daemon_memory_bytes"),
                "home_disk_bytes": of("home_disk_bytes"), "home_disk_bytes_at_end": of("home_disk_bytes_at_end"),
                "floor_ms": floor, "ratio_to_floor": ratio,
            }));
        }
    }
    out
}

fn print_table(it: &Args, summary: &[Value], named: &Value) {
    let mib = |v: &Value| v["median"].as_f64().map_or("-".to_owned(), |b| format!("{:.0}", b / 1048576.0));
    println!(
        "{SCRIPT}: blaze {} (engine {}) at {}{}, project {}, {} reps, `blaze {}`",
        named["binary"]["hash"].as_str().unwrap_or("?"),
        named["binary"]["engine_hash"].as_str().unwrap_or("-"),
        named["tree"]["commit"].as_str().unwrap_or("?"),
        if named["tree"]["dirty"] == true { " (dirty)" } else { "" },
        named["project"]["root"].as_str().unwrap_or("?"),
        it.reps,
        it.command
    );
    println!("{:<13} {:<8} {:>28} {:>8} {:>12} {:>11} {:>10}", "op", "host", "wall ms median [min..max]", "/floor", "client MiB", "daemon MiB", "disk MiB");
    for row in summary {
        let wall = &row["wall_ns"];
        let ms = |key: &str| wall[key].as_f64().map_or("-".to_owned(), |ns| format!("{:.0}", ns / 1e6));
        let ratio = row["ratio_to_floor"].as_f64().map_or("-".to_owned(), |r| format!("{r:.2}"));
        println!(
            "{:<13} {:<8} {:>28} {:>8} {:>12} {:>11} {:>10}",
            row["op"].as_str().unwrap_or(""),
            row["host"].as_str().unwrap_or(""),
            format!("{} [{}..{}]", ms("median"), ms("min"), ms("max")),
            ratio,
            mib(&row["client_peak_rss_bytes"]),
            mib(&row["daemon_memory_bytes"]),
            mib(&row["home_disk_bytes"]),
        );
    }
}

fn selftest() {
    let dir = TempDir::new("in-process-selftest-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    expect(SCRIPT, spread(&[3.0, 1.0, 2.0]) == Some((2.0, 1.0, 3.0)), "the median of three is the middle one");
    expect(SCRIPT, spread(&[4.0, 1.0, 2.0, 3.0]) == Some((2.5, 1.0, 4.0)), "the median of four is the mean of the middle two");
    expect(SCRIPT, spread(&[]).is_none(), "no values have no median");
    let file = dir.path().join("lib.rs");
    expect(SCRIPT, std::fs::write(&file, "fn f(x: u32) -> u32 { x + 1 }\n").is_ok(), "the selftest's file is written");
    let edit = Edit { file: "lib.rs".into(), find: "x + 1".into(), replace: vec!["x + 2".into()], comment: "//".into() };
    let slot = edit.slot();
    expect(SCRIPT, replace_once(&file, &slot[0], &slot[1]).is_ok(), "the first body edit applies");
    expect(SCRIPT, replace_once(&file, &slot[1], &slot[0]).is_ok(), "the second body edit cycles back");
    expect(SCRIPT, replace_once(&file, "x + 9", "x").is_err(), "a text not there is refused");
    let parsed = input::read(r#"{"project": "p", "blaze": "b", "timeout_s": 5, "edit": {"file": "f", "find": "a", "replace": ["b"]}, "floor_ms": {"body": 50}}"#)
        .and_then(|value| input::of::<Args>(&value, ""));
    expect(SCRIPT, parsed.as_ref().is_ok_and(|it| it.hosts == HOSTS && it.floor_ms.get("body") == Some(&50.0)), "the example parses, both hosts by default");
    for bad in [
        r#"{"project": "p", "blaze": "b", "timeout_s": 5, "edit": {"file": "f", "find": "a", "replace": ["a"]}}"#,
        r#"{"project": "p", "blaze": "b", "timeout_s": 5, "edit": {"file": "f", "find": "a", "replace": ["b"]}, "floor_ms": {"warm": 1}}"#,
        r#"{"project": "p", "blaze": "b", "timeout_s": 0, "edit": {"file": "f", "find": "a", "replace": ["b"]}}"#,
        r#"{"project": "p", "blaze": "b", "timeout_s": 5, "edit": {"file": "f", "find": "a", "replace": ["b"]}, "host": "process"}"#,
    ] {
        expect(SCRIPT, input::read(bad).and_then(|value| input::of::<Args>(&value, "")).is_err(), &format!("refused: {bad}"));
    }
    let log = dir.path().join("true");
    let ran = measure(&["/bin/sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()], dir.path(), &[], &log, Duration::from_secs(10));
    expect(SCRIPT, ran.as_ref().is_ok_and(|it| it.status == 3 && it.rss_bytes > 0), "a child's status and peak RSS come back from wait4");
    let slow = measure(&["/bin/sh".to_owned(), "-c".to_owned(), "sleep 5".to_owned()], dir.path(), &[], &log, Duration::from_secs(1));
    expect(SCRIPT, slow.is_err(), "a child past its bound is killed and said");
    expect(SCRIPT, disk_bytes(dir.path()) > 0, "a directory with files takes bytes on disk");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}
