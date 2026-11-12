//! The part that touches the machine: a test's package written into a directory of its own,
//! `blaze check` and `blaze build` asked of one warm daemon, the built program run in blaze's
//! sandbox, each with a time limit. What they did goes to `judge.rs`, which decides.
//!
//! The corpus is untrusted data. Its sources are only compiled by blaze (proc macros in
//! blaze's wasm sandbox, as any macro) and its programs only run through `blaze sandbox`: the
//! machine read-only, no network, writes kept to the run's own overlay. Where that sandbox
//! cannot be made, the run's own failure is the test's result; nothing runs outside it.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use blaze_script::{Value, json};

use super::clean::{self, Hint};
use super::judge::{self, Compiled, Diagnostic, Ran, Verdict};
use super::plan::{Crate, Kind, Plan, Suite, manifest};

/// Where and how blaze runs.
pub(crate) struct Engine {
    /// The client (`blaze`).
    pub blaze: PathBuf,
    /// `blaze-engine` beside it, whose `sandbox` command runs a program.
    pub engine: PathBuf,
    pub home: PathBuf,
    pub cache: PathBuf,
    pub tmp: PathBuf,
    /// The daemon's memory ceiling, GiB (`BLAZE_DAEMON_MEMORY_GB`).
    pub memory_gb: i64,
    /// `BLAZE_CORES`.
    pub cores: i64,
    /// The limit of one `blaze` command and of one program run.
    pub limit: Duration,
}

/// A program that ran to its end or to its limit.
pub(crate) struct Out {
    /// `None`: a signal ended it, or it was stopped at its limit.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Runs `command` to its end or `limit`, both outputs read while it runs. At the limit it is
/// sent `SIGTERM` (blaze's sandbox passes it on to the program), then killed.
fn bounded(mut command: Command, limit: Duration) -> Result<Out, String> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{:?} did not start: {e}", command.get_program()))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|it| Box::new(it) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|it| Box::new(it) as Box<dyn Read + Send>));
    let deadline = Instant::now() + limit;
    // Bounded: it ends at the deadline at the latest, polling every 5 ms.
    let status = loop {
        match child.try_wait().map_err(|e| format!("waiting: {e}"))? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let timed_out = status.is_none();
    if timed_out {
        signal("-TERM", child.id());
        let grace = Instant::now() + Duration::from_secs(2);
        // Bounded: two seconds for the signal to end it, then the kill.
        while Instant::now() < grace && child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    Ok(Out { status: status.and_then(|it| it.code()), stdout: text(stdout.join().unwrap_or_default()), stderr: text(stderr.join().unwrap_or_default()), timed_out })
}

/// `kill <how> <pid>`: whether it was delivered.
fn signal(how: &str, pid: u32) -> bool {
    #[allow(clippy::disallowed_methods, reason = "a script signals a process it started, by pid")]
    let sent = Command::new("kill").args([how, &pid.to_string()]).stderr(Stdio::null()).status();
    sent.is_ok_and(|it| it.success())
}

/// What one test's run left, beyond its verdict.
#[derive(Default)]
pub(crate) struct Notes {
    /// `rustc_clean` hints held, broken (each named), and with no blaze meaning.
    pub hints_held: usize,
    pub hints_not_held: usize,
    /// Functions a test says changed that blaze did not make again: reported, not a failure
    /// (the same code can come of another body).
    pub reused_though_changed: Vec<String>,
    /// `blaze` commands run.
    pub commands: usize,
}

impl Engine {
    fn command(&self, program: &Path, args: &[&str], cwd: &Path) -> Command {
        #[allow(clippy::disallowed_methods, reason = "the one place this script starts blaze; the environment is given whole and `cwd` always")]
        let mut command = Command::new(program);
        command.env_clear();
        // The caller's `HOME` (the toolchain blaze fetched is the machine's) and `PATH` (the C
        // driver blaze's linker asks for the C runtime's files); nothing else is inherited.
        for name in ["HOME", "PATH"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("TMPDIR", &self.tmp)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("BLAZE_LOCK", "write")
            .env("BLAZE_DAEMON_MEMORY_GB", self.memory_gb.to_string())
            .env("BLAZE_CORES", self.cores.to_string())
            .env("RUST_BACKTRACE", "0")
            .args(args)
            .current_dir(cwd);
        command
    }

    /// `blaze <args>` in `cwd`, within `limit`.
    pub(crate) fn blaze(&self, args: &[&str], cwd: &Path, limit: Duration) -> Result<Out, String> {
        bounded(self.command(&self.blaze, args, cwd), limit)
    }

    /// The daemon's pid and its memory, as `blaze status --json` says; `None` with no daemon.
    pub(crate) fn daemon(&self) -> Option<(u32, u64)> {
        let out = self.blaze(&["status", "--json"], &self.tmp, Duration::from_secs(30)).ok()?;
        let value: Value = json::from_str(out.stdout.trim()).ok()?;
        let daemon = value.get("daemon")?;
        Some((u32::try_from(daemon.get("pid")?.as_u64()?).ok()?, daemon.get("memory_bytes").and_then(Value::as_u64).unwrap_or(0)))
    }

    /// `blaze stop`, and the daemon's process gone: a command started while it ends would reach
    /// a dying daemon. A daemon that does not go within two minutes is killed, and said.
    pub(crate) fn stop(&self) -> Result<String, String> {
        let Some((pid, memory)) = self.daemon() else { return Ok("no daemon was running".to_owned()) };
        let _ = self.blaze(&["stop"], &self.tmp, Duration::from_secs(120))?;
        let deadline = Instant::now() + Duration::from_secs(120);
        // Bounded: two minutes, then the kill.
        while signal("-0", pid) {
            if Instant::now() >= deadline {
                signal("-KILL", pid);
                std::thread::sleep(Duration::from_secs(1));
                return Ok(format!("the daemon (pid {pid}) outlived `blaze stop` by 120 s and was killed; it held {} MiB", memory >> 20));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(format!("the daemon (pid {pid}) stopped; it held {} MiB", memory >> 20))
    }

    /// One `blaze check` or `blaze build` of the package in `dir`; `main` is how the test's own
    /// file ends as blaze names it. With it: the program it built, and the build's id.
    fn compile(&self, action: &str, dir: &Path, main: &str, notes: &mut Notes) -> (Compiled, Option<String>, Option<u64>) {
        notes.commands += 1;
        let out = match self.blaze(&[action, "--message-format=json"], dir, self.limit) {
            Ok(out) => out,
            Err(why) => return (Compiled { said: why.clone(), panic: Some(format!("blaze did not run: {why}")), ..Compiled::default() }, None, None),
        };
        let mut compiled = Compiled { accepted: out.status == Some(0), timed_out: out.timed_out, said: out.stderr.clone(), ..Compiled::default() };
        let (mut executable, mut build) = (None, None);
        for line in out.stdout.lines().filter(|it| it.starts_with("{\"reason\"")) {
            let Ok(value) = json::from_str::<Value>(line) else { continue };
            match value["reason"].as_str() {
                Some("compiler-artifact") => {
                    if let Some(path) = value["executable"].as_str() {
                        executable = Some(path.to_owned());
                    }
                }
                Some("build-finished") => build = value["build_id"].as_u64(),
                Some("compiler-message") => {
                    let message = &value["message"];
                    if let Some(rendered) = message["rendered"].as_str() {
                        compiled.said.push_str(rendered);
                    }
                    compiled.diagnostics.push(diagnostic(message, main));
                }
                _ => {}
            }
        }
        compiled.panic = panic_line(&out, &compiled);
        (compiled, executable, build)
    }

    /// The functions whose code build `build` made again, by the last part of their paths, as
    /// `blaze why --all --json` lists them; `None` when it answers for another build.
    fn redone(&self, dir: &Path, build: Option<u64>, notes: &mut Notes) -> Option<Vec<String>> {
        // No build id: the build was answered as the last one was, and made nothing.
        let Some(build) = build else { return Some(Vec::new()) };
        notes.commands += 1;
        let out = self.blaze(&["why", "--all", "--json"], dir, self.limit).ok()?;
        let value: Value = json::from_str(out.stdout.trim()).ok()?;
        if value["build"].as_u64() != Some(build) {
            return None;
        }
        let mut made = Vec::new();
        for group in value["groups"].as_array()? {
            if group["step"].as_str().is_some_and(|it| it.starts_with("codegen")) {
                made.extend(group["items"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|it| it.rsplit("::").next().unwrap_or(it).to_owned()));
            }
        }
        Some(made)
    }

    /// The program run in blaze's sandbox, with `dir` as an overlay it may write for itself.
    fn run(&self, dir: &Path, executable: &str, plan: &Plan) -> Ran {
        let cwd = dir.join("run");
        let spec = dir.join("sandbox.json");
        let mut env = vec![json::json!(["PATH", "/usr/bin:/bin"]), json::json!(["RUST_BACKTRACE", "0"])];
        env.extend(plan.run.env.iter().map(|(name, value)| json::json!([name, value])));
        let text = json::json!({"cwd": cwd, "env": env, "overlays": [dir], "writable": [], "readable": [], "record": null, "record_reads": false, "not_inputs": []});
        if let Err(why) = std::fs::create_dir_all(&cwd).and_then(|()| std::fs::write(&spec, text.to_string())) {
            return Ran { stderr: format!("the sandbox's spec could not be written: {why}"), status: Some(-1), ..Ran::default() };
        }
        let spec_text = spec.to_string_lossy().into_owned();
        let mut args = vec!["sandbox", spec_text.as_str(), "--", executable];
        args.extend(plan.run.args.iter().map(String::as_str));
        match bounded(self.command(&self.engine, &args, dir), self.limit) {
            Ok(out) => Ran { status: out.status, stdout: out.stdout, stderr: out.stderr, timed_out: out.timed_out },
            Err(why) => Ran { stderr: format!("the sandbox did not start: {why}"), status: Some(-1), ..Ran::default() },
        }
    }

    /// One revision's plan carried out in `dir`, where its files already are.
    fn carry_out(&self, dir: &Path, plan: &Plan, expected_stdout: Option<&str>, notes: &mut Notes) -> (Verdict, Option<u64>, bool) {
        let name = plan.main.file.rsplit('/').next().unwrap_or(&plan.main.file);
        let main = format!("t/{name}");
        match plan.kind {
            Kind::CheckPass => (judge::accepted("check", plan, &self.compile("check", dir, &main, notes).0).unwrap_or(Verdict::Pass), None, false),
            Kind::BuildPass => {
                let (compiled, _, build) = self.compile("build", dir, &main, notes);
                (judge::accepted("build", plan, &compiled).unwrap_or(Verdict::Pass), build, true)
            }
            Kind::RunPass | Kind::RunFail(_) => {
                let (compiled, executable, build) = self.compile("build", dir, &main, notes);
                if let Some(verdict) = judge::accepted("build", plan, &compiled) {
                    return (verdict, build, true);
                }
                let Some(executable) = executable else {
                    return (Verdict::Fail { cause: "build: no program is named in its messages".to_owned(), detail: judge_tail(&compiled.said) }, build, true);
                };
                (judge::ran(plan, &self.run(dir, &executable, plan), expected_stdout), build, true)
            }
            Kind::CompileFail { build } => {
                if let Some(verdict) = judge::refused("check", plan, &self.compile("check", dir, &main, notes).0) {
                    return (verdict, None, false);
                }
                if build && let Some(verdict) = judge::refused("build", plan, &self.compile("build", dir, &main, notes).0) {
                    return (verdict, None, false);
                }
                (judge::accepts(plan), None, false)
            }
            Kind::Survive => {
                let (checked, _, _) = self.compile("check", dir, &main, notes);
                if let Some(verdict) = judge::survived("check", &checked) {
                    return (verdict, None, false);
                }
                if checked.accepted && let Some(verdict) = judge::survived("build", &self.compile("build", dir, &main, notes).0) {
                    return (verdict, None, false);
                }
                (Verdict::Pass, None, false)
            }
        }
    }

    /// A whole test in `dir`: one plan for a ui or crashes test, one a revision for an
    /// incremental one, each built on what the one before left. `root` is the suite's directory.
    pub(crate) fn test(&self, suite: Suite, root: &Path, dir: &Path, plans: &[(Option<String>, Plan)], notes: &mut Notes) -> Verdict {
        let harness = |what: &str, why: std::io::Error| Verdict::Fail { cause: format!("harness: {what}"), detail: why.to_string() };
        let _ = std::fs::remove_dir_all(dir);
        let Some((_, first)) = plans.first() else { return Verdict::Pass };
        let source = match std::fs::read_to_string(root.join(&first.main.file)) {
            Ok(it) => it,
            Err(why) => return harness("the test could not be read", why),
        };
        let hints = if suite == Suite::Incremental { clean::hints(&source) } else { Vec::new() };
        notes.hints_not_held += hints.iter().filter(|it| matches!(it, Hint::NotHeld { .. })).count();
        for (index, (revision, plan)) in plans.iter().enumerate() {
            let placed = match index {
                0 => place(root, dir, plan, suite == Suite::Incremental),
                // The files are there: the revision is the manifest's `--cfg`.
                _ => std::fs::write(dir.join("Cargo.toml"), manifest(&plan.main, &format!("t/{}", file_name(&plan.main.file)), Some(plan))),
            };
            if let Err(why) = placed {
                return harness("the test's package could not be written", why);
            }
            let expected = plan.run.stdout.as_ref().map(|it| std::fs::read_to_string(root.join(it)).unwrap_or_default());
            let (verdict, build, built) = self.carry_out(dir, plan, expected.as_deref(), notes);
            let label = revision.as_deref().unwrap_or("");
            let named = |detail: String| if suite == Suite::Incremental { format!("revision {label}: {detail}") } else { detail };
            match verdict {
                Verdict::Pass => {}
                Verdict::Fail { cause, detail } => return Verdict::Fail { cause, detail: named(detail) },
                Verdict::Panic { cause, detail } => return Verdict::Panic { cause, detail: named(detail) },
                Verdict::Timeout { step } => return Verdict::Timeout { step },
            }
            let mine: Vec<&Hint> = hints
                .iter()
                .filter(|it| matches!(it, Hint::Reused { revision, .. } | Hint::Remade { revision, .. } if revision == label))
                .collect();
            if mine.is_empty() {
                continue;
            }
            // A revision that only checks makes no code, and the first build makes all of it.
            let made = if built && index > 0 { self.redone(dir, build, notes) } else { None };
            let Some(made) = made else {
                notes.hints_not_held += mine.len();
                continue;
            };
            for hint in mine {
                match hint {
                    Hint::Reused { function, .. } if made.iter().any(|it| it == function) => {
                        return Verdict::Fail {
                            cause: "incremental: makes again a function the test marks clean".to_owned(),
                            detail: format!("revision {label}: `{function}` is `#[rustc_clean]` with no `except`, and its code was made again"),
                        };
                    }
                    Hint::Remade { function, .. } if !made.iter().any(|it| it == function) => {
                        notes.reused_though_changed.push(format!("{label}: {function}"));
                    }
                    _ => notes.hints_held += 1,
                }
            }
        }
        Verdict::Pass
    }
}

fn judge_tail(said: &str) -> String {
    let text = said.trim();
    let from = text.char_indices().rev().nth(399).map_or(0, |it| it.0);
    text[from..].to_owned()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The first line that says blaze itself broke: a panic it reports, or a client a signal ended.
fn panic_line(out: &Out, compiled: &Compiled) -> Option<String> {
    const MARKS: [&str; 2] = ["panicked at ", "panicked: "];
    let marked = |text: &str| text.lines().find(|line| MARKS.iter().any(|mark| line.contains(mark))).map(|line| line.trim().to_owned());
    // In what blaze prints, only a panic's own line (`panicked at <place>`): a program's
    // diagnostics are there too, and a constant's `evaluation panicked: ..` is one of them.
    if let Some(line) = out.stderr.lines().find(|line| line.contains(MARKS[0])) {
        return Some(line.trim().to_owned());
    }
    // A diagnostic about no place in the source that says so: the daemon's report of a job.
    if let Some(it) = compiled.diagnostics.iter().find(|it| it.lines.is_empty() && MARKS.iter().any(|mark| it.message.contains(mark))) {
        return marked(&it.message).or_else(|| Some(it.message.clone()));
    }
    (out.status.is_none() && !out.timed_out).then(|| "blaze was ended by a signal".to_owned())
}

/// One of blaze's JSON diagnostics; `main` is how the test's own file ends.
fn diagnostic(message: &Value, main: &str) -> Diagnostic {
    let mut lines = Vec::new();
    for span in message["spans"].as_array().into_iter().flatten().filter(|it| it["is_primary"] == true) {
        // The span, then each macro call it was expanded from.
        let mut at = Some(span);
        // bound: an expansion chain deeper than 64 is cut.
        for _ in 0..64 {
            let Some(it) = at else { break };
            if it["file_name"].as_str().is_some_and(|name| name.ends_with(main))
                && let Some(line) = it["line_start"].as_u64()
            {
                lines.push(line as usize);
            }
            at = it.get("expansion").and_then(|it| it.get("span")).filter(|it| it.is_object());
        }
    }
    lines.sort_unstable();
    lines.dedup();
    Diagnostic {
        level: message["level"].as_str().unwrap_or("").to_owned(),
        code: message["code"]["code"].as_str().map(str::to_owned),
        message: message["message"].as_str().unwrap_or("").to_owned(),
        lines,
    }
}

/// Writes `it`'s root file into `into`, and beside it links to what is beside the original
/// when the source names such files. `blank`: with rustc's graph assertions written over.
fn place_crate(root: &Path, it: &Crate, into: &Path, blank: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(into)?;
    let from = root.join(&it.file);
    let name = file_name(&it.file);
    match blank {
        true => std::fs::write(into.join(name), clean::blanked(&std::fs::read_to_string(&from)?))?,
        false => std::fs::copy(&from, into.join(name)).map(|_| ())?,
    }
    if !it.siblings {
        return Ok(());
    }
    let Some(beside) = from.parent() else { return Ok(()) };
    let mut entries: Vec<PathBuf> = std::fs::read_dir(beside)?.filter_map(|it| it.ok().map(|it| it.path())).collect();
    entries.sort();
    for entry in entries {
        let Some(entry_name) = entry.file_name() else { continue };
        if entry_name != name {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&entry, into.join(entry_name))?;
        }
    }
    Ok(())
}

/// The test's package in `dir`: its file under `t/`, each auxiliary a package under `aux/`.
fn place(root: &Path, dir: &Path, plan: &Plan, blank: bool) -> std::io::Result<()> {
    place_crate(root, &plan.main, &dir.join("t"), blank)?;
    std::fs::write(dir.join("Cargo.toml"), manifest(&plan.main, &format!("t/{}", file_name(&plan.main.file)), Some(plan)))?;
    for aux in &plan.aux {
        let into = dir.join("aux").join(&aux.name);
        place_crate(root, aux, &into, blank)?;
        std::fs::write(into.join("Cargo.toml"), manifest(aux, file_name(&aux.file), None))?;
    }
    Ok(())
}

/// A hello world built and run for each profile the plans use, so the standard library is
/// built before any test's limit starts: `Err` says what blaze could not do.
pub(crate) fn warm(engine: &Engine, work: &Path, profiles: &BTreeMap<String, String>, limit: Duration) -> Result<(), String> {
    for (name, profile) in profiles {
        let dir = work.join("warm").join(name);
        let written = std::fs::create_dir_all(dir.join("src"))
            .and_then(|()| std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"warm\");\n}\n"))
            .and_then(|()| std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"warm\"\nversion = \"0.0.0\"\nedition = \"2021\"\n{profile}\n[workspace]\n")));
        written.map_err(|why| format!("{}: {why}", dir.display()))?;
        for action in ["check", "build"] {
            let out = engine.blaze(&[action, "--message-format=json"], &dir, limit)?;
            if out.status != Some(0) {
                let why = if out.timed_out { format!("did not end in {} s", limit.as_secs()) } else { format!("exited {:?}", out.status) };
                return Err(format!("`blaze {action}` of a hello world (profile `{name}`) {why}:\n{}\n{}", judge_tail(&out.stdout), judge_tail(&out.stderr)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_is_read_with_the_lines_of_the_tests_own_file() {
        let message: Value = json::from_str(
            r#"{"message":"mismatched types","code":{"code":"E0308","explanation":null},"level":"error","spans":[
                {"file_name":"t/a.rs","line_start":7,"is_primary":true,"expansion":null},
                {"file_name":"t/a.rs","line_start":3,"is_primary":false,"expansion":null},
                {"file_name":"/rustc/x/library/core/src/macros.rs","line_start":40,"is_primary":true,
                 "expansion":{"span":{"file_name":"/w/t/a.rs","line_start":12,"is_primary":false,"expansion":null},"macro_decl_name":"m!"}}],
               "children":[],"rendered":"error"}"#,
        )
        .unwrap();
        let read = diagnostic(&message, "t/a.rs");
        assert_eq!(read, Diagnostic { level: "error".to_owned(), code: Some("E0308".to_owned()), message: "mismatched types".to_owned(), lines: vec![7, 12] });
    }

    #[test]
    fn a_panic_is_told_from_a_programs_error() {
        let out = |stderr: &str, status: Option<i32>| Out { status, stdout: String::new(), stderr: stderr.to_owned(), timed_out: false };
        let none = Compiled::default();
        assert_eq!(panic_line(&out("error: could not compile `a`\n", Some(101)), &none), None);
        assert_eq!(
            panic_line(&out("  thread 'worker' panicked at crates/x.rs:1:2: boom\nnote: more\n", Some(101)), &none).as_deref(),
            Some("thread 'worker' panicked at crates/x.rs:1:2: boom")
        );
        assert_eq!(panic_line(&out("", None), &none).as_deref(), Some("blaze was ended by a signal"));
        let const_eval = Compiled {
            diagnostics: vec![Diagnostic { level: "error".to_owned(), code: Some("E0080".to_owned()), message: "evaluation panicked: explicit panic".to_owned(), lines: vec![3] }],
            ..Compiled::default()
        };
        assert_eq!(panic_line(&out("", Some(101)), &const_eval), None);
        let job = Compiled {
            diagnostics: vec![Diagnostic { level: "error".to_owned(), code: None, message: "the unit's job panicked: index out of bounds".to_owned(), lines: Vec::new() }],
            ..Compiled::default()
        };
        assert!(panic_line(&out("", Some(101)), &job).is_some());
    }
}
