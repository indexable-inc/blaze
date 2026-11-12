//! The real driver: a test's revisions as real processes. One daemon in a home of its own
//! serves every revision, its incremental state kept between them; then each revision is
//! built from nothing at the same paths (a fresh home, cache and checkout, so the bytes can
//! be compared) and the two are held together (`check.rs`).
//!
//! It is for what only a kernel shows (the file watcher's event order, `kill -9`, a program
//! really run) and as the cross-check of the simulated driver. What it reads of a build is
//! blaze's own account: the `blaze-work` and `build-finished` lines of
//! `--message-format=json`, `blaze why --all --json` (the functions and units made again)
//! and `blaze explain edit_work` (the engine's memos run and verified).
//!
//! Every command runs under one fixed environment (`env_clear` and a short list), in a root
//! off any symlink (`/private/tmp`, never `/tmp`: a home named through a link turns the
//! no-op lane off), with the store check forced on (`BLAZE_VERIFY_STORE=1`), and its exit
//! status and output are read.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use super::{
    check::{self, Row},
    spec::{Change, Observed, Revision, Step, Test},
};

/// Longer than any command here takes with the standard library built cold.
const DEADLINE: Duration = Duration::from_secs(600);

/// The most tests that run at once, each with one daemon alive: a daemon's store pool
/// reserves a large address range, and many at once fail with ENOMEM. `BLAZE_REVISIONS_JOBS`
/// sets another bound.
const JOBS: u32 = 4;

static RUNNING: (Mutex<u32>, Condvar) = (Mutex::new(0), Condvar::new());

struct Slot;

/// The bound on tests at once.
pub fn jobs() -> u32 {
    std::env::var("BLAZE_REVISIONS_JOBS").ok().and_then(|it| it.parse().ok()).unwrap_or(JOBS)
}

impl Slot {
    fn take() -> Slot {
        let most = jobs();
        let mut running = RUNNING.0.lock().unwrap();
        while *running >= most {
            running = RUNNING.1.wait(running).unwrap();
        }
        *running += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *RUNNING.0.lock().unwrap_or_else(|it| it.into_inner()) -= 1;
        RUNNING.1.notify_one();
    }
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
    ms: u128,
}

struct Real {
    blaze: PathBuf,
    root: PathBuf,
    /// The checkout, the daemon's home and the machine's cache: the same paths for the
    /// incremental daemon and for each clean build.
    dir: PathBuf,
    home: PathBuf,
    cache: PathBuf,
    /// Every checkout a `project` step made beside the first, each set aside with it.
    others: Vec<PathBuf>,
    /// How much of the daemon's log was read for panics.
    log_read: u64,
    /// Whether a failure leaves the root in place, for its logs.
    keep: bool,
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn alive(pid: &str) -> bool {
    Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
}

impl Real {
    fn new(test: &Test) -> Real {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // The daemon's socket is `<home>/v2.sock`, at most 104 bytes on macOS; and
        // `/private/tmp`, not `/tmp`, which is a link there.
        let tmp = if Path::new("/private/tmp").is_dir() { "/private/tmp" } else { "/tmp" };
        let root = Path::new(tmp).join(format!("brv-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("test"), &test.name).unwrap();
        let blaze = std::env::var_os("BLAZE_REVISIONS_BLAZE").map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_blaze")), PathBuf::from);
        Real { blaze, dir: root.join("w"), home: root.join("h"), cache: root.join("c"), others: Vec::new(), root, log_read: 0, keep: true }
    }

    fn blaze(&self, args: &[&str], env: &BTreeMap<String, String>) -> Out {
        let mut command = Command::new(&self.blaze);
        command.env_clear();
        for name in ["HOME", "BLAZE_DUCKDB"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TMPDIR", self.root.join("t"))
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("BLAZE_LOCK", "write")
            .env("BLAZE_VERIFY_STORE", "1")
            .env("RUST_BACKTRACE", "0")
            .envs(env)
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let began = Instant::now();
        let mut child = command.spawn().unwrap_or_else(|error| panic!("starting {}: {error}", self.blaze.display()));
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() > DEADLINE {
                child.kill().unwrap();
                child.wait().unwrap();
                self.kill_daemon();
                panic!(
                    "`blaze {}` in {} ran {} s without finishing: a hang (the daemon's log: {})",
                    args.join(" "),
                    self.dir.display(),
                    DEADLINE.as_secs(),
                    self.home.join("v2/serve.log").display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        Out { ok: status.success(), stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap(), ms: began.elapsed().as_millis() }
    }

    /// The pid of the daemon the home's log last says is listening.
    fn daemon_pid(&self) -> Option<String> {
        let log = fs::read_to_string(self.home.join("v2/serve.log")).ok()?;
        let pid = log.rsplit("listening on ").next()?.split("(pid ").nth(1)?.split(')').next()?;
        (!pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then(|| pid.to_owned())
    }

    /// `kill -9` of the daemon, by its pid, waited for until the process is gone: a command
    /// started meanwhile would reach the dying daemon.
    fn kill_daemon(&self) {
        let Some(pid) = self.daemon_pid() else { return };
        let _ = Command::new("kill").args(["-KILL", &pid]).stderr(Stdio::null()).status();
        let began = Instant::now();
        while alive(&pid) {
            assert!(began.elapsed() < Duration::from_secs(30), "pid {pid} outlived its kill -9 by 30 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// `blaze stop`, and the daemon's process gone: `stop` answers before the process has
    /// exited, and a daemon started meanwhile at the same home has ended at once (exit 1,
    /// "its command line was refused", 2026-11-11).
    fn stop(&self) {
        let pid = self.daemon_pid();
        let out = self.blaze(&["stop"], &BTreeMap::new());
        assert!(out.ok, "`blaze stop` failed:\n{}", out.stderr);
        let began = Instant::now();
        while pid.as_deref().is_some_and(alive) {
            assert!(began.elapsed() < Duration::from_secs(60), "the daemon (pid {pid:?}) outlived its `blaze stop` by 60 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn write(&self, changes: &[Change], how: &Step) {
        for change in changes {
            let path = self.dir.join(&change.path);
            let Some(bytes) = &change.bytes else {
                fs::remove_file(&path).unwrap_or_else(|error| panic!("removing {}: {error}", path.display()));
                continue;
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            match how {
                Step::EditByRename => {
                    let beside = path.with_extension("saved-by-rename");
                    fs::write(&beside, bytes).unwrap();
                    fs::rename(&beside, &path).unwrap();
                }
                Step::EditTruncateWrite => {
                    let mut file = fs::File::create(&path).unwrap();
                    file.sync_all().unwrap();
                    file.write_all(bytes).unwrap();
                }
                _ => fs::write(&path, bytes).unwrap(),
            }
        }
    }

    /// Panics the daemon's log holds past what was read before.
    fn panics(&mut self) -> (u64, String) {
        let Ok(log) = fs::read(self.home.join("v2/serve.log")) else { return (0, String::new()) };
        // A new daemon may start its log again.
        let from = if (log.len() as u64) < self.log_read { 0 } else { self.log_read as usize };
        self.log_read = log.len() as u64;
        let new = String::from_utf8_lossy(&log[from..]).into_owned();
        let mut lines = new.lines().filter(|line| line.contains("panicked at"));
        match lines.next() {
            Some(first) => (1 + lines.count() as u64, first.to_owned()),
            None => (0, String::new()),
        }
    }

    /// The revision's action run, and what blaze says of it. `account`: whether the
    /// daemon is asked what it made again (the incremental build), or only the outcome is
    /// read (the clean one).
    fn act(&mut self, revision: &Revision, env: &BTreeMap<String, String>, account: bool) -> Observed {
        let mut args = vec![revision.action.word(), "--message-format=json"];
        args.extend(revision.args.iter().map(String::as_str));
        let out = self.blaze(&args, env);
        let mut seen = Observed { ok: out.ok, said: out.stderr, ms: out.ms, ..Observed::default() };
        // Each line of standard output that is one of blaze's JSON messages is the build's;
        // every other line is the program's.
        let mut build = None;
        let mut executable = None;
        for line in out.stdout.split_inclusive('\n') {
            let value = match line.starts_with("{\"reason\"") {
                true => serde_json::from_str::<serde_json::Value>(line).ok(),
                false => None,
            };
            let Some(value) = value else {
                seen.stdout.push_str(line);
                continue;
            };
            match value["reason"].as_str() {
                Some("blaze-work") => {
                    seen.units = value["compiled"].as_u64();
                    seen.link = Some(match value["links"].as_array().and_then(|it| it.last()) {
                        Some(link) => link["how"].as_str().unwrap_or("?").to_owned(),
                        None => "none".to_owned(),
                    });
                }
                Some("build-finished") => {
                    build = value["build_id"].as_u64();
                    seen.noop = Some(value["build_id"].is_null());
                }
                Some("compiler-artifact") => {
                    if let Some(path) = value["executable"].as_str() {
                        executable = Some(path.to_owned());
                    }
                }
                Some("compiler-message") => {
                    if let Some(rendered) = value["message"]["rendered"].as_str() {
                        seen.said.push_str(rendered);
                    }
                }
                _ => {}
            }
        }
        (seen.panics, seen.first_panic) = self.panics();
        if revision.expect.bytes && seen.ok {
            seen.image = executable.and_then(|path| fs::read(path).ok());
        }
        if !account {
            return seen;
        }
        if seen.noop == Some(true) {
            // No build ran: nothing was made again, and no memo ran.
            (seen.redone, seen.executed, seen.verified) = (Some(Vec::new()), Some(0), Some(0));
            seen.records = Some(0);
            seen.why = "answered as the last build was (the no-op lane)".to_owned();
            return seen;
        }
        let none = BTreeMap::new();
        let why = self.blaze(&["why", "--all", "--json"], &none);
        match serde_json::from_str::<serde_json::Value>(why.stdout.trim()) {
            Ok(value) if why.ok && value["build"].as_u64() == build && build.is_some() => {
                let mut redone = Vec::new();
                let mut records = 0;
                for group in value["groups"].as_array().into_iter().flatten() {
                    if group["step"].as_str() == Some("unit") {
                        records += group["items"].as_array().map_or(0, |it| it.len() as u64);
                    }
                    if group["step"].as_str().is_some_and(|it| it.starts_with("codegen")) {
                        redone.extend(group["items"].as_array().into_iter().flatten().filter_map(|it| it.as_str()).map(str::to_owned));
                    }
                }
                redone.sort();
                redone.dedup();
                seen.redone = Some(redone);
                seen.records = Some(records);
            }
            _ => seen.why = format!("`blaze why --all --json` of build {build:?} said: {}{}\n", why.stdout, why.stderr),
        }
        seen.why.push_str(&self.blaze(&["why", "--all"], &none).stdout);
        if let Some(build) = build
            && (revision.expect.max_executed.is_some() || revision.expect.max_verified.is_some())
        {
            let work = self.blaze(&["explain", "edit_work", &format!("build={build}"), "limit=500", "--json"], &none);
            for line in work.stdout.lines() {
                let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if row["stage"] == "engine memos" {
                    match row["counter"].as_str() {
                        Some("executed") => seen.executed = row["total"].as_u64(),
                        Some("verified") => seen.verified = row["total"].as_u64(),
                        _ => {}
                    }
                }
            }
            if work.stderr.contains("counted no work in any stage") {
                (seen.executed, seen.verified) = (Some(0), Some(0));
            }
            if seen.executed.is_none() {
                seen.why.push_str(&format!("`blaze explain edit_work build={build}` said: {}{}\n", work.stdout, work.stderr));
            }
        }
        seen
    }

    fn step(&mut self, step: &Step, revision: &Revision, env: &BTreeMap<String, String>) {
        match step {
            Step::Edit | Step::EditByRename | Step::EditTruncateWrite => self.write(&revision.changes, step),
            Step::Stop => self.stop(),
            Step::Kill => {
                assert!(self.daemon_pid().is_some(), "`kill`: the daemon's log names no pid");
                self.kill_daemon();
            }
            Step::Touch(path) => {
                let file = fs::File::options().write(true).open(self.dir.join(path)).unwrap_or_else(|error| panic!("touch {path}: {error}"));
                file.set_modified(SystemTime::now()).unwrap();
            }
            Step::Project(name) => self.enter(Some(name)),
            Step::Home(name) => self.rehome(name),
            Step::Do(action) => {
                // The revision's own arguments: a request that differs in them is another
                // request to the no-op lane.
                let mut args = vec![action.word(), "--message-format=json"];
                args.extend(revision.args.iter().map(String::as_str));
                let out = self.blaze(&args, env);
                assert!(out.ok, "{}: the step `{}` failed:\n{}", revision.label, action.word(), out.stderr);
            }
        }
    }

    /// Commands run in the checkout of the project `name` from here on (`project NAME`), or
    /// in the first (`None`): a directory beside the first, under the same home and daemon.
    fn enter(&mut self, name: Option<&str>) {
        let dir = self.root.join(match name {
            // Never the first checkout's, the homes' or the scratch directory's name.
            Some(name) => format!("p-{name}"),
            None => "w".to_owned(),
        });
        if name.is_some() && !self.others.contains(&dir) {
            self.others.push(dir.clone()); // clone: a path, once a project
        }
        fs::create_dir_all(&dir).unwrap();
        self.dir = dir;
    }

    /// Commands run under the home `h-<name>` and the machine directory `c-<name>` from here
    /// on (`home NAME`): the daemon stopped, and the new home given a copy of the store it
    /// left, nothing else of the old home (no database image, no build directory, no ledger).
    fn rehome(&mut self, name: &str) {
        self.stop();
        let (home, cache) = (self.root.join(format!("h-{name}")), self.root.join(format!("c-{name}")));
        let store = self.home.join("v2/store");
        assert!(store.is_dir(), "`home {name}`: the first home has no store at {}", store.display());
        copy_dir(&store, &home.join("v2/store"));
        for dir in [&self.home, &self.cache] {
            if !self.others.contains(dir) {
                self.others.push(dir.clone()); // clone: a path, once a home
            }
        }
        (self.home, self.cache) = (home, cache);
        self.log_read = 0;
    }

    /// The homes and every checkout set aside under `as_`, so the next build starts from
    /// nothing at the same paths.
    fn set_aside(&mut self, as_: &str) {
        let first = self.root.join("w");
        for dir in [&first, &self.home, &self.cache].into_iter().chain(&self.others) {
            if dir.exists() {
                let aside = dir.with_extension(as_);
                let _ = fs::remove_dir_all(&aside);
                fs::rename(dir, aside).unwrap();
            }
        }
        self.log_read = 0;
    }
}

/// `from`, a directory, copied to `to` with everything under it.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap_or_else(|error| panic!("reading {}: {error}", from.display())) {
        let entry = entry.unwrap();
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&source, &target),
            false => {
                fs::copy(&source, &target).unwrap_or_else(|error| panic!("copying {}: {error}", source.display()));
            }
        }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        if self.home.exists() {
            self.kill_daemon();
        }
        if !thread::panicking() || !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// When a revision is also built from nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Clean {
    /// Every revision: the suite's cases.
    Always,
    /// Only a revision whose incremental build failed a check, to tell a build that is wrong
    /// only incrementally from one blaze gets wrong from nothing too: a corpus of hundreds.
    WhenWrong,
}

/// `test`'s rows under the real driver, and where its logs are.
pub fn rows(test: &Test, clean_when: Clean) -> (Vec<Row>, String) {
    let _slot = Slot::take();
    let mut real = Real::new(test);
    real.keep = clean_when == Clean::Always;
    fs::create_dir_all(real.root.join("t")).unwrap();
    let mut rows: Vec<Row> = Vec::new();
    for (index, revision) in test.revisions.iter().enumerate() {
        let env = test.env_at(index);
        for step in &revision.steps {
            real.step(step, revision, &env);
        }
        let seen = real.act(revision, &env, true);
        let failed = check::held(revision, &seen, None, &[]);
        rows.push(Row { label: revision.label.clone(), seen, failed, clean_ok: None });
    }
    real.stop();
    real.set_aside("incremental");
    // Each revision from nothing, at the same paths. Two revisions of one state, action and
    // environment share one clean build.
    let mut built: Vec<(usize, Observed)> = Vec::new();
    for (index, revision) in test.revisions.iter().enumerate() {
        if clean_when == Clean::WhenWrong && rows[index].failed.is_empty() {
            continue;
        }
        let env = test.env_at(index);
        let same = built.iter().find(|(earlier, _)| {
            let before = &test.revisions[*earlier];
            test.files_at(*earlier) == test.files_at(index)
                && test.env_at(*earlier) == env
                && before.action == revision.action
                && before.args == revision.args
                && before.expect.bytes == revision.expect.bytes
        });
        let clean = match same {
            // clone: one clean build's outcome, read by a second revision
            Some((_, clean)) => clean.clone(),
            None => {
                // In its project's checkout, at the path the incremental build had it.
                real.enter(test.project_at(index).1);
                let files: Vec<Change> =
                    test.files_at(index).into_iter().map(|(path, bytes)| Change { path, bytes: Some(bytes) }).collect();
                real.write(&files, &Step::Edit);
                let clean = real.act(revision, &env, false);
                real.stop();
                real.set_aside("clean");
                clean
            }
        };
        rows[index].failed = check::held(revision, &rows[index].seen, Some(&clean), &[]);
        rows[index].clean_ok = Some(clean.ok);
        built.push((index, clean));
    }
    let note = format!(
        "the daemon's log: {}; the last clean build's: {}\n",
        real.home.with_extension("incremental").join("v2/serve.log").display(),
        real.home.with_extension("clean").join("v2/serve.log").display()
    );
    (rows, note)
}

/// `test` under the real driver.
pub fn run(test: &Test) {
    let (rows, note) = rows(test, Clean::Always);
    check::verdict(test, "real", &rows, &note);
}
