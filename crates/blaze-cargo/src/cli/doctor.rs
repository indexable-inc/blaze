//! `blaze doctor [--json]`: what is wrong with blaze here, checked without changing anything,
//! each check with what it found and the command that fixes it.
//!
//! Its sections, in order:
//! - `version`: this blaze against the workspace's pin (`[workspace.metadata.blaze] version`),
//!   and against the newest release the daemon's last update check kept
//!   (`<blaze home>/update/state.json`): no network is asked.
//! - `agents`: what `blaze agents status` says, for each agent used here or installed for.
//! - `daemon`: who holds the state (`<state>/serve.lock`), and whether the daemon running is
//!   another build of blaze than this one.
//! - `state`: the daemon image (`<state>/db/image`: an image of this blaze's layout whose spine
//!   hashes to what it names, every group against its CRC32C, [`blaze_image::Image::verify`]),
//!   the profile history (its manifest reads, [`blaze_profile::history::read_manifest`]), the
//!   state's size against the disk's free bytes, and directories of layouts this blaze no longer
//!   reads (`v2.store-format-*`, `runner-*` under blaze's root), with their sizes.
//! - `toolchain`: the Rust blaze builds with (`blaze toolchain`), with its library sources, and
//!   the machine's shared directory.
//! - `project`: large directories of the workspace that are not `target/` or `.git/` and have no
//!   `CACHEDIR.TAG`, and a file system whose timestamps have no sub-second part (the watcher's
//!   racy window is then [`blaze_vfs::COARSE_NS`], not [`blaze_vfs::FINE_NS`]).
//! - `tools`: the sandbox build scripts and proc macros run in ([`crate::sandbox::available`]).
//!
//! Each check is `ok`, `absent` or `unused` (no fault: a part a daemon starts empty), `warn`
//! (blaze works, worse or older than it could) or `fault` (it does not); `fix` is the command
//! that fixes it. It runs in the CLI, on the files: a daemon that cannot start is when it is
//! needed. It deletes and moves nothing: the command it prints for a faulty state moves the
//! faulty directory aside whole, so it is kept for a bug report.
//!
//! The answer is the first line; the exit code is nonzero only for a fault. `--json` prints
//! [`Report`], version [`VERSION`]: `{"version": 2, "state": DIR, "ok": BOOL, "checks":
//! [{"section", "what", "file", "verdict", "said", "fix"}], "quarantine": [COMMAND]}`.
//! Version 2 added `section`, `fix` and the `warn` verdict, and the sections past `state`.

use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use blaze_core::{Mapping, exit::Exit};
use blaze_image::{Image, OpenError};
use serde::Serialize;

use super::say;

const USAGE: &str = "blaze doctor [--json]";

/// The version of [`Report`]'s JSON: bumped when a field changes meaning or goes away.
pub const VERSION: u32 = 2;

/// What `blaze doctor` found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub version: u32,
    /// The state directory checked.
    pub state: String,
    /// No check found a fault.
    pub ok: bool,
    pub checks: Vec<Check>,
    /// The commands that set each faulty part aside, in the order to run them; empty when
    /// `ok`.
    pub quarantine: Vec<String>,
}

/// One part of the persisted state, and what checking it found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    /// The section it is in: `version`, `agents`, `daemon`, `state`, `toolchain`, `project`,
    /// `tools`.
    pub section: &'static str,
    /// What was checked: `daemon image`, `profile history`, `pin`, ...
    pub what: &'static str,
    /// The file or directory it is about (`""` for none).
    pub file: String,
    pub verdict: Verdict,
    /// What was found, in words: counts for `ok`, the rule for `unused`, the fault for `fault`.
    pub said: String,
    /// The command that fixes it; `None` when it is sound, or nothing blaze has fixes it.
    pub fix: Option<String>,
}

impl Check {
    fn new(section: &'static str, what: &'static str, file: &Path, verdict: Verdict, said: String) -> Check {
        Check { section, what, file: file.display().to_string(), verdict, said, fix: None }
    }

    fn fixed_by(self, fix: impl Into<String>) -> Check {
        Check { fix: Some(fix.into()), ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// It is there and every check of it held.
    Ok,
    /// It is not there: a daemon starts that part empty, which is no fault.
    Absent,
    /// It is whole, and this blaze does not use it by a rule (another blaze's layout): a
    /// daemon starts that part empty and says so. No fault.
    Unused,
    /// blaze works with it, worse or older than it could: an update out, an agent's install
    /// out of date, a directory the watcher walks for nothing.
    Warn,
    /// It is not what a blaze writes, or fails a check: blaze refuses to start on it or cannot
    /// do its work.
    Fault,
}

/// `blaze doctor [--json]`.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let mut json = false;
    for word in words {
        match word.to_str() {
            Some("--json") => json = true,
            _ => {
                blaze_report::err!(
                    "{}",
                    say::error(&format!("{USAGE}, not `{}`", word.to_string_lossy()), color)
                );
                return Exit::Usage.code();
            }
        }
    }
    let state = match crate::serve::state() {
        Ok(state) => state,
        Err(error) => {
            blaze_report::err!("{}", say::error(&format!("`blaze doctor`: {error:#}"), color));
            return Exit::Failed.code();
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_secs());
    let report = examine_all(&state, now);
    let said = match json {
        true => match serde_json::to_string(&report) {
            Ok(mut line) => {
                line.push('\n');
                line
            }
            Err(error) => {
                blaze_report::err!(
                    "{}",
                    say::error(&format!("`blaze doctor`'s report as JSON: {error}"), color)
                );
                return Exit::Failed.code();
            }
        },
        false => text(&report),
    };
    blaze_report::out!("{said}");
    match report.ok {
        true => 0,
        false => Exit::Failed.code(),
    }
}

/// The state under `state` checked; `now` (seconds since the epoch) names where a faulty part
/// would be set aside.
pub fn examine(state: &Path, now: u64) -> Report {
    let db = state.join("db");
    let history = blaze_profile::history::dir_in(state);
    let mut image = image(&image_file(state));
    let mut profile = profile(&history);
    let mut quarantine = Vec::new();
    for (check, dir) in [(&mut image, &db), (&mut profile, &history)] {
        if check.verdict == Verdict::Fault {
            let aside = set_aside(dir, now);
            check.fix = Some(format!("blaze stop && {aside}"));
            quarantine.push(aside);
        }
    }
    if !quarantine.is_empty() {
        // First, so no daemon holds or writes what is moved.
        quarantine.insert(0, "blaze stop".to_owned());
    }
    Report {
        version: VERSION,
        state: state.display().to_string(),
        ok: quarantine.is_empty(),
        checks: vec![image, profile],
        quarantine,
    }
}

/// The command that moves `dir` aside, whole, to a name beside it no check reads.
fn set_aside(dir: &Path, now: u64) -> String {
    let mut aside = dir.as_os_str().to_owned();
    aside.push(format!(".suspect-{now}"));
    format!("mv {} {}", quoted(dir), quoted(Path::new(&aside)))
}

/// `path` as one word of a POSIX shell.
fn quoted(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = text.bytes().all(|it| it.is_ascii_alphanumeric() || b"/._-+,:@".contains(&it));
    match plain && !text.is_empty() {
        true => text,
        false => format!("'{}'", text.replace('\'', "'\\''")),
    }
}

/// The daemon image at `file`, checked whole.
fn image(file: &Path) -> Check {
    let check = |verdict, said: String| Check::new("state", "daemon image", file, verdict, said);
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return check(
                Verdict::Absent,
                "no image: the next daemon starts with an empty database".to_owned(),
            );
        }
        Err(error) => return check(Verdict::Fault, format!("it cannot be read: {error}")),
    };
    let len = bytes.len();
    // Copied to a 16-byte boundary, as a mapped file's pages are.
    let mapping = Mapping::copy(&bytes);
    drop(bytes);
    let opened = match Image::open(mapping) {
        Ok(opened) => opened,
        Err(error @ OpenError::Layout { .. }) => {
            return check(
                Verdict::Unused,
                format!("{error}: another blaze wrote it, and this one starts empty rather than read it"),
            );
        }
        Err(error) => return check(Verdict::Fault, error.to_string()),
    };
    let corrupt = opened.verify();
    if let Some(first) = corrupt.first() {
        let more = match corrupt.len() {
            1 => String::new(),
            n => format!(" ({} more groups fail their checksum)", n - 1),
        };
        return check(Verdict::Fault, format!("{first}{more}"));
    }
    let sections = opened.sections().len();
    check(
        Verdict::Ok,
        format!("{} in {sections} sections, every group's checksum holds", size(len)),
    )
}

/// The profile history in `dir`: its manifest read, as every reader of it does.
fn profile(dir: &Path) -> Check {
    let manifest = dir.join("manifest.json");
    let check =
        |verdict, said: String| Check::new("state", "profile history", &manifest, verdict, said);
    if !manifest.exists() {
        return check(Verdict::Absent, "no history: no daemon has flushed a profile here".to_owned());
    }
    match blaze_profile::history::read_manifest(dir) {
        Ok(read) => check(
            Verdict::Ok,
            format!("its manifest reads, at flush {}", read.flushed()),
        ),
        Err(error) => check(Verdict::Fault, error.to_string()),
    }
}

/// A size in bytes, for a person.
fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{} KiB", bytes >> 10),
        _ => format!("{} MiB", bytes >> 20),
    }
}

/// `report` for a person: the answer, a line a check, then the commands that set the faulty
/// parts aside.
pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let count = |verdict| report.checks.iter().filter(|it| it.verdict == verdict).count();
    let (faults, warnings) = (count(Verdict::Fault), count(Verdict::Warn));
    let plural = |n: usize, word: &str| match n {
        1 => format!("1 {word}"),
        n => format!("{n} {word}s"),
    };
    // Writing to a `String` does not fail.
    _ = match (faults, warnings) {
        (0, 0) => writeln!(out, "blaze is sound here (state {})", report.state),
        (0, w) => writeln!(out, "blaze works here, with {} (state {})", plural(w, "warning"), report.state),
        (f, w) => writeln!(out, "{}, {} (state {})", plural(f, "fault"), plural(w, "warning"), report.state),
    };
    let mut section = "";
    for it in &report.checks {
        if it.section != section {
            section = it.section;
            _ = writeln!(out, "{section}:");
        }
        let verdict = match it.verdict {
            Verdict::Ok => "ok",
            Verdict::Absent => "absent",
            Verdict::Unused => "unused",
            Verdict::Warn => "WARN",
            Verdict::Fault => "FAULT",
        };
        match it.file.is_empty() {
            true => _ = writeln!(out, "  {verdict:<6}  {}: {}", it.what, it.said),
            false => _ = writeln!(out, "  {verdict:<6}  {} {}: {}", it.what, it.file, it.said),
        }
        if let Some(fix) = &it.fix {
            _ = writeln!(out, "          fix: {fix}");
        }
    }
    if !report.quarantine.is_empty() {
        _ = writeln!(
            out,
            "to set it aside (kept whole, for a bug report; the next daemon starts that part empty and builds cold):"
        );
        _ = writeln!(out, "  {}", report.quarantine.join(" && "));
    }
    out
}

/// Where the state's daemon image is, for a caller naming it.
pub fn image_file(state: &Path) -> PathBuf {
    state.join("db").join("image")
}

/// Every section checked: [`examine`]'s state, and around it what this machine, this blaze and
/// the workspace run in say. Reads files and asks the running daemon which blaze it is; writes
/// nothing.
pub fn examine_all(state: &Path, now: u64) -> Report {
    let mut report = examine(state, now);
    let state_checks = std::mem::take(&mut report.checks);
    let ours = crate::release::get().version;
    let project = std::env::current_dir().ok().and_then(|cwd| super::agents::project_of(&cwd).ok().flatten());
    let mut checks = Vec::new();

    // version
    let manifest = project.as_ref().map(|it| it.join("Cargo.toml"));
    let pinned = manifest.as_deref().map(|it| (it, std::fs::read_to_string(it)));
    checks.push(match &pinned {
        None => pin_check(ours, None),
        Some((file, Ok(text))) => pin_check(ours, Some((*file, pin_of(text)))),
        Some((file, Err(error))) => {
            Check::new("version", "pin", file, Verdict::Warn, format!("it cannot be read: {error}"))
        }
    });
    match blaze_manifest::user_config::home() {
        Ok(home) => {
            let file = home.join("update").join("state.json");
            checks.push(update_check(ours, &file, read_optional(&file).as_deref()));
        }
        Err(why) => checks.push(Check::new("version", "update", Path::new(""), Verdict::Warn, format!("blaze has no home to keep its update check in: {why}"))),
    }

    // agents
    match super::agents::statuses() {
        Ok(statuses) => checks.extend(agents_checks(&statuses)),
        Err(why) => checks.push(Check::new("agents", "agents", Path::new(""), Verdict::Warn, why)),
    }

    // daemon
    let lock = state.join("serve.lock");
    checks.push(lock_check(&lock, read_optional(&lock).as_deref()));
    checks.push(build_check(super::which::asked_other().map_err(|error| format!("{error:#}"))));

    // state
    checks.extend(state_checks);
    let mut budget = WALK_BUDGET;
    let used = size_of(state, &mut budget);
    #[cfg(unix)]
    let free = crate::raw::system::free_bytes(state);
    #[cfg(not(unix))]
    let free = None;
    checks.push(disk_check(state, used, free));
    if let Some(root) = state.parent() {
        let mut old = Vec::new();
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if OLD_LAYOUTS.iter().any(|it| name.starts_with(it)) {
                let mut budget = WALK_BUDGET;
                old.push((entry.path(), size_of(&entry.path(), &mut budget)));
            }
        }
        old.sort();
        checks.push(old_layouts_check(root, &old));
    }

    // toolchain
    let sysroot = crate::toolchain::found();
    let library = sysroot.as_ref().map(|it| it.dir().join("lib/rustlib/src/rust/library"));
    checks.push(toolchain_check(sysroot.as_ref().map(|it| it.dir()), library.as_deref().is_some_and(Path::is_dir)));
    checks.push(match crate::serve::machine() {
        Ok(dir) => {
            let there = dir.is_dir();
            shared_check(&dir, there)
        }
        Err(error) => Check::new("toolchain", "shared directory", Path::new(""), Verdict::Warn, format!("{error:#}")),
    });

    // project
    if let Some(root) = &project {
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|it| it.is_dir()) || SKIPPED.contains(&name.as_str()) {
                continue;
            }
            let tagged = entry.path().join("CACHEDIR.TAG").is_file();
            let mut budget = WALK_BUDGET;
            let size = if tagged { Measured::default() } else { size_of(&entry.path(), &mut budget) };
            dirs.push((name, size, tagged));
        }
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        checks.push(untagged_check(root, &dirs));
        let manifest = root.join("Cargo.toml");
        checks.push(match std::fs::metadata(&manifest) {
            Ok(meta) => stamps_check(&manifest, stamp_of(&meta)),
            Err(error) => Check::new("project", "file times", &manifest, Verdict::Warn, format!("it cannot be read: {error}")),
        });
    }

    // tools
    checks.push(sandbox_check(crate::sandbox::available(false)));

    report.ok = !checks.iter().any(|it| it.verdict == Verdict::Fault);
    report.checks = checks;
    report
}

/// The text of `file`, `None` when it can't be read (absent or not).
fn read_optional(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

/// The version `[workspace.metadata.blaze]` pins in a root manifest's text, `None` for none.
pub fn pin_of(text: &str) -> Option<String> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let version = table.get("workspace")?.get("metadata")?.get("blaze")?.get("version")?;
    version.as_str().map(str::to_owned)
}

/// A release's version as numbers to order by: `0.YYYYMMDD.N`.
fn version_key(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|it| it.parse::<u64>().ok());
    let key = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(key)
}

const DEV: &str = "0.0.0-dev";

/// This blaze (`ours`) against the version the workspace's manifest pins.
pub fn pin_check(ours: &str, pinned: Option<(&Path, Option<String>)>) -> Check {
    let check = |file: &Path, verdict, said: String| Check::new("version", "pin", file, verdict, said);
    match pinned {
        None => check(Path::new(""), Verdict::Absent, "not run in a Cargo workspace: no pin to hold this blaze to".to_owned()),
        Some((file, None)) => check(file, Verdict::Ok, format!("the workspace pins no blaze: blaze {ours} runs here")),
        Some((file, Some(pin))) if pin == ours => check(file, Verdict::Ok, format!("the workspace pins blaze {pin}, which this is")),
        Some((file, Some(pin))) if ours == DEV => check(
            file,
            Verdict::Ok,
            format!("the workspace pins blaze {pin}; this is a build of the tree, which runs itself in blaze's own tree"),
        ),
        Some((file, Some(pin))) => check(
            file,
            Verdict::Warn,
            format!("the workspace pins blaze {pin}, and this is {ours}: builds here run {pin}, and what this doctor says is of {ours}"),
        )
        .fixed_by(format!("run the `blaze` the launcher installs, which runs {pin} here; or move the pin with `blaze pin {ours}`")),
    }
}

/// The newest release of `channel` in the daemon's kept `latest.json`, from the text of
/// `update/state.json`; `None` when no `latest.json` was kept.
fn latest_in(text: &str) -> Result<(Option<String>, Option<String>), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("it is not JSON: {error}"))?;
    if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("it has no `schema` 1".to_owned());
    }
    let failed = value.get("failed").and_then(serde_json::Value::as_str).map(str::to_owned);
    let latest = value.get("latest").filter(|it| !it.is_null());
    let stable = latest.and_then(|it| it.get("stable")?.get("version")?.as_str()).map(str::to_owned);
    Ok((stable, failed))
}

/// This blaze (`ours`) against the newest stable release the daemon's last update check kept
/// in `file` (`text` its text, `None` when there is none). No network is asked.
pub fn update_check(ours: &str, file: &Path, text: Option<&str>) -> Check {
    let check = |verdict, said: String| Check::new("version", "update", file, verdict, said);
    if ours == DEV {
        return check(Verdict::Ok, "a build of the tree: releases do not apply".to_owned());
    }
    let Some(text) = text else {
        return check(
            Verdict::Absent,
            "no update check has been kept: a daemon checks latest.json once it runs, unless `[update] mode` is off".to_owned(),
        );
    };
    match latest_in(text) {
        Err(why) => check(Verdict::Warn, format!("{why}: the next daemon's check writes it again")),
        Ok((_, Some(failed))) => check(Verdict::Warn, format!("the last update check failed: {failed}")),
        Ok((None, None)) => check(Verdict::Absent, "the last update check kept no latest.json".to_owned()),
        Ok((Some(newest), None)) => match (version_key(&newest), version_key(ours)) {
            (Some(newest_key), Some(ours_key)) if newest_key > ours_key => {
                check(Verdict::Warn, format!("blaze {newest} is out, and this is {ours}")).fixed_by("blaze update")
            }
            _ => check(Verdict::Ok, format!("blaze {ours} is current (the newest stable is {newest})")),
        },
    }
}

/// What `blaze agents status` says, as checks: one for each agent and scope it is used at or
/// installed in. An agent used here with nothing of blaze's in either scope is a warning once.
pub fn agents_checks(statuses: &[super::agents::Status]) -> Vec<Check> {
    let mut checks = Vec::new();
    for status in statuses {
        let installed = !status.after.is_empty() || !status.before.is_empty() || status.block.is_some();
        let elsewhere = statuses.iter().any(|it| {
            it.agent == status.agent && it.scope != status.scope && (it.current || !it.after.is_empty())
        });
        if !installed && (!status.found || elsewhere || status.scope == "project") {
            continue;
        }
        let verdict = if status.current { Verdict::Ok } else { Verdict::Warn };
        let said = format!("{} ({} scope): {}", status.agent, status.scope, status.verdict);
        let check = Check::new("agents", "agent", Path::new(&status.hooks), verdict, said);
        checks.push(match &status.fix {
            Some(fix) => check.fixed_by(fix.clone()),
            None => check,
        });
    }
    checks
}

/// Who holds the state, from the text of its `serve.lock` (`None`: no file): the pid of the
/// daemon that holds it, or that stopped last (`<pid> stopped`).
pub fn lock_check(file: &Path, text: Option<&str>) -> Check {
    let check = |verdict, said: String| Check::new("daemon", "lock", file, verdict, said);
    let Some(text) = text else {
        return check(Verdict::Absent, "no daemon has held this state".to_owned());
    };
    let mut words = text.split_whitespace();
    match (words.next().map(str::parse::<u32>), words.next()) {
        (None, _) => check(Verdict::Ok, "no daemon has written its pid: none holds it, or one is starting".to_owned()),
        (Some(Ok(pid)), Some("stopped")) => check(Verdict::Ok, format!("the last daemon (pid {pid}) stopped and wrote its image")),
        (Some(Ok(pid)), None) => check(
            Verdict::Ok,
            format!("pid {pid} holds the state, or held it and ended without its stop (a crash: the next daemon restores the last image)"),
        ),
        _ => check(Verdict::Warn, format!("it says `{}`, which no blaze writes", text.trim())).fixed_by("blaze stop"),
    }
}

/// Whether the daemon running is another build of blaze than this one ([`super::which`]).
pub fn build_check(other: Result<Option<String>, String>) -> Check {
    let check = |verdict, said: String| Check::new("daemon", "build", Path::new(""), verdict, said);
    match other {
        Ok(None) => check(Verdict::Ok, "no daemon of another build runs".to_owned()),
        Ok(Some(line)) => check(Verdict::Warn, line).fixed_by("blaze stop"),
        Err(why) => check(Verdict::Warn, format!("the daemon could not be asked which blaze it is: {why}")).fixed_by("blaze stop"),
    }
}

/// The most directory entries one size walk reads before it says "at least" (bounded-progress).
const WALK_BUDGET: u64 = 1_000_000;

/// The directories under blaze's root of layouts this blaze no longer reads.
const OLD_LAYOUTS: [&str; 2] = ["v2.store-format-", "runner-"];

/// The directories of a workspace that are expected to be large: never said untagged.
const SKIPPED: [&str; 2] = ["target", ".git"];

/// A directory's bytes, and whether the walk stopped at its budget (then at least that).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Measured {
    pub bytes: u64,
    pub capped: bool,
}

impl Measured {
    fn said(self) -> String {
        let at_least = if self.capped { "at least " } else { "" };
        format!("{at_least}{}", size(usize::try_from(self.bytes).unwrap_or(usize::MAX)))
    }
}

/// The bytes of the files under `dir`, links not followed, reading at most `budget` entries.
fn size_of(dir: &Path, budget: &mut u64) -> Measured {
    let mut sized = Measured::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.flatten() {
            if *budget == 0 {
                sized.capped = true;
                return sized;
            }
            *budget -= 1;
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => stack.push(entry.path()),
                Ok(meta) => sized.bytes = sized.bytes.saturating_add(meta.len()),
                Err(_) => {}
            }
        }
    }
    sized
}

/// The state's size against the free bytes of its disk: a fault under 1 GiB free (a build
/// writes more than that), a warning under 10 GiB or a quarter of the state (a collection and
/// the next image need room to write before they free).
pub fn disk_check(state: &Path, used: Measured, free: Option<u64>) -> Check {
    const GIB: u64 = 1 << 30;
    let check = |verdict, said: String| Check::new("state", "disk", state, verdict, said);
    let Some(free) = free else {
        return check(Verdict::Warn, format!("the state is {}; the disk's free bytes could not be read", used.said()));
    };
    let free_said = Measured { bytes: free, capped: false }.said();
    let said = format!("the state is {}, the disk has {free_said} free", used.said());
    if free < GIB {
        check(Verdict::Fault, said).fixed_by("blaze cache gc")
    } else if free < 10 * GIB || free < used.bytes / 4 {
        check(Verdict::Warn, said).fixed_by("blaze cache gc")
    } else {
        check(Verdict::Ok, said)
    }
}

/// Directories under blaze's root `root` of layouts this blaze no longer reads, with sizes.
pub fn old_layouts_check(root: &Path, old: &[(PathBuf, Measured)]) -> Check {
    let check = |verdict, said: String| Check::new("state", "old layouts", root, verdict, said);
    if old.is_empty() {
        return check(Verdict::Ok, "no directory of an old layout".to_owned());
    }
    let named: Vec<String> = old.iter().map(|(dir, size)| format!("{} ({})", dir.display(), size.said())).collect();
    let words: Vec<String> = old.iter().map(|(dir, _)| quoted(dir)).collect();
    check(Verdict::Warn, format!("no blaze of this layout reads {}", named.join(", ")))
        .fixed_by(format!("rm -rf {}", words.join(" ")))
}

/// The Rust blaze builds with: found, with its library's sources.
pub fn toolchain_check(sysroot: Option<&Path>, library: bool) -> Check {
    match (sysroot, library) {
        (None, _) => Check::new("toolchain", "rust", Path::new(""), Verdict::Warn, "not downloaded: the first build downloads it".to_owned())
            .fixed_by("blaze toolchain"),
        (Some(dir), true) => Check::new("toolchain", "rust", dir, Verdict::Ok, "found, with its library's sources".to_owned()),
        (Some(dir), false) => Check::new(
            "toolchain",
            "rust",
            dir,
            Verdict::Fault,
            "found without its library's sources (lib/rustlib/src/rust/library): std cannot be built".to_owned(),
        )
        .fixed_by(format!("rm -rf {} && blaze toolchain", quoted(dir))),
    }
}

/// The machine's shared directory (what every daemon of the user shares, [`crate::serve::machine`]).
pub fn shared_check(dir: &Path, there: bool) -> Check {
    match there {
        true => Check::new("toolchain", "shared directory", dir, Verdict::Ok, "there".to_owned()),
        false => Check::new("toolchain", "shared directory", dir, Verdict::Absent, "not made yet: the first daemon makes it".to_owned()),
    }
}

/// The directories at a workspace's root `root` (name, size, tagged) of a gigabyte or more that
/// have no `CACHEDIR.TAG` and are not `target/` or `.git/`: the watcher walks them, and backups
/// copy them.
pub fn untagged_check(root: &Path, dirs: &[(String, Measured, bool)]) -> Check {
    const LARGE: u64 = 1 << 30;
    let check = |verdict, said: String| Check::new("project", "large directories", root, verdict, said);
    let large: Vec<&(String, Measured, bool)> = dirs.iter().filter(|(_, size, tagged)| !tagged && size.bytes >= LARGE).collect();
    if large.is_empty() {
        return check(Verdict::Ok, "no directory of a gigabyte or more without a CACHEDIR.TAG".to_owned());
    }
    let named: Vec<String> = large.iter().map(|(name, size, _)| format!("{name}/ ({})", size.said())).collect();
    let tags: Vec<String> = large.iter().map(|(name, _, _)| format!("{}/CACHEDIR.TAG", quoted(&root.join(name)))).collect();
    check(Verdict::Warn, format!("{} with no CACHEDIR.TAG: the watcher walks it", named.join(", "))).fixed_by(format!(
        "printf 'Signature: 8a477f597d28d172789f06886806bc55\\n' | tee {} >/dev/null, if it is a cache",
        tags.join(" ")
    ))
}

/// A file's stamp, as the watcher reads one.
#[cfg(unix)]
fn stamp_of(meta: &std::fs::Metadata) -> blaze_core::Stamp {
    use std::os::unix::fs::MetadataExt as _;
    let ns = |secs: i64, nanos: i64| {
        u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000_000).saturating_add(u64::try_from(nanos).unwrap_or(0))
    };
    blaze_core::Stamp {
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        mode: meta.mode(),
    }
}

#[cfg(not(unix))]
fn stamp_of(meta: &std::fs::Metadata) -> blaze_core::Stamp {
    let ns = meta
        .modified()
        .ok()
        .and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX));
    blaze_core::Stamp { ino: 0, size: meta.len(), mtime_ns: ns, ctime_ns: ns, mode: 0 }
}

/// Whether the workspace's file system keeps sub-second times, from one file's stamp: one with
/// none in its mtime and ctime both has the watcher wait [`blaze_vfs::COARSE_NS`] after each
/// change before its stamp vouches for anything ([`blaze_vfs::window_ns`]).
pub fn stamps_check(file: &Path, stamp: blaze_core::Stamp) -> Check {
    let check = |verdict, said: String| Check::new("project", "file times", file, verdict, said);
    match blaze_vfs::window_ns(&stamp) == blaze_vfs::COARSE_NS {
        false => check(Verdict::Ok, "the file system keeps sub-second times".to_owned()),
        true => check(
            Verdict::Warn,
            format!(
                "its mtime and ctime have no sub-second part: on a file system of whole seconds (HFS+, FAT, some network ones) \
                 an edit is trusted only {} s after it, so edits read later; a file that landed on a whole second says the same",
                blaze_vfs::COARSE_NS / 1_000_000_000
            ),
        )
        .fixed_by("move the workspace to a file system with nanosecond times (APFS, ext4, btrfs, xfs, NTFS)"),
    }
}

/// The sandbox build scripts and proc macros run in, made around nothing
/// ([`crate::sandbox::available`]).
pub fn sandbox_check(probed: Result<(), String>) -> Check {
    match probed {
        Ok(()) => Check::new("tools", "sandbox", Path::new(""), Verdict::Ok, "a sandbox can be made".to_owned()),
        Err(why) => Check::new(
            "tools",
            "sandbox",
            Path::new(""),
            Verdict::Fault,
            format!("no sandbox can be made, so no build script or proc macro can run: {why}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use blaze_format::SchemaHash;
    use blaze_ids::ContentId;
    use blaze_image::{Builder, SectionId};

    use super::*;

    /// An image of one section, as a daemon writes the plane's history.
    fn an_image() -> Vec<u8> {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, SchemaHash(ContentId([3; 32])), &[7; 5000]);
        builder.finish().expect("a small image")
    }

    fn state_with(image: Option<&[u8]>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(bytes) = image {
            std::fs::create_dir_all(dir.path().join("db")).unwrap();
            std::fs::write(image_file(dir.path()), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn an_empty_state_is_sound_and_says_what_is_absent() {
        let dir = state_with(None);
        let report = examine(dir.path(), 7);
        assert!(report.ok && report.quarantine.is_empty(), "{report:?}");
        assert!(report.checks.iter().all(|it| it.verdict == Verdict::Absent), "{report:?}");
        let said = text(&report);
        assert!(said.starts_with("blaze is sound here (state "), "{said}");
        assert!(said.contains("\nstate:\n  absent  daemon image "), "{said}");
    }

    #[test]
    fn a_whole_image_is_sound() {
        let dir = state_with(Some(&an_image()));
        let report = examine(dir.path(), 7);
        assert!(report.ok, "{report:?}");
        assert_eq!(report.checks[0].verdict, Verdict::Ok, "{report:?}");
        assert!(report.checks[0].said.contains("every group's checksum holds"), "{report:?}");
    }

    #[test]
    fn a_flipped_byte_is_a_fault_naming_the_file_and_the_command_that_sets_it_aside() {
        let mut bytes = an_image();
        // The section's first byte.
        let at = Image::open(bytes.clone()).expect("an image").sections()[0].at as usize;
        assert_eq!(bytes[at], 7);
        bytes[at] ^= 0x40;
        let dir = state_with(Some(&bytes));
        let report = examine(dir.path(), 1_762_850_000);
        assert!(!report.ok, "{report:?}");
        let image = &report.checks[0];
        assert_eq!(image.verdict, Verdict::Fault, "{report:?}");
        assert!(image.file.ends_with("db/image") && image.said.contains("is corrupt"), "{image:?}");
        let db = dir.path().join("db").display().to_string();
        assert_eq!(report.quarantine, ["blaze stop".to_owned(), format!("mv {db} {db}.suspect-1762850000")]);
        // Nothing was moved or deleted.
        assert!(image_file(dir.path()).exists());
        let said = text(&report);
        assert!(said.starts_with("1 fault, 0 warnings (state "), "{said}");
        assert!(said.contains(&format!("  blaze stop && mv {db} {db}.suspect-1762850000\n")), "{said}");
        assert_eq!(image.fix.as_deref(), Some(format!("blaze stop && mv {db} {db}.suspect-1762850000").as_str()));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["version"], 2);
        assert_eq!(json["checks"][0]["verdict"], "fault");
        assert_eq!(json["checks"][0]["section"], "state");
    }

    #[test]
    fn a_file_that_is_no_image_is_a_fault() {
        let dir = state_with(Some(b"not an image"));
        let report = examine(dir.path(), 7);
        assert_eq!(report.checks[0].verdict, Verdict::Fault, "{report:?}");
        assert!(report.checks[0].said.contains("not a blaze image"), "{report:?}");
    }

    // Each check of the sections past `state`: a sound fixture, and a faulty one it names.

    #[test]
    fn the_pin_is_held_to_this_blaze() {
        let file = Path::new("/w/Cargo.toml");
        let text = "[workspace]\nmembers = []\n[workspace.metadata.blaze]\nversion = \"0.20261101.2\"\n";
        assert_eq!(pin_of(text).as_deref(), Some("0.20261101.2"));
        assert_eq!(pin_of("[workspace]\n"), None);
        assert_eq!(pin_check("0.20261101.2", Some((file, pin_of(text)))).verdict, Verdict::Ok);
        let other = pin_check("0.20261110.1", Some((file, pin_of(text))));
        assert_eq!(other.verdict, Verdict::Warn, "{other:?}");
        assert!(other.said.contains("pins blaze 0.20261101.2, and this is 0.20261110.1"), "{other:?}");
        assert_eq!(pin_check(DEV, Some((file, pin_of(text)))).verdict, Verdict::Ok);
    }

    #[test]
    fn a_newer_release_kept_by_the_last_update_check_is_said_with_its_fix() {
        let file = Path::new("/h/update/state.json");
        let kept = r#"{"schema": 1, "latest": {"schema": 1, "stable": {"version": "0.20261112.1"}}, "failed": null}"#;
        let newer = update_check("0.20261101.3", file, Some(kept));
        assert_eq!((newer.verdict, newer.fix.as_deref()), (Verdict::Warn, Some("blaze update")), "{newer:?}");
        assert_eq!(update_check("0.20261112.1", file, Some(kept)).verdict, Verdict::Ok);
        assert_eq!(update_check("0.20261112.1", file, None).verdict, Verdict::Absent);
        let failed = r#"{"schema": 1, "latest": null, "failed": "no network"}"#;
        assert!(update_check("0.20261112.1", file, Some(failed)).said.contains("no network"));
        assert_eq!(update_check("0.20261112.1", file, Some("{")).verdict, Verdict::Warn);
        assert_eq!(version_key("0.20261112.10"), Some((0, 20_261_112, 10)));
        assert!(version_key("0.20261112.10") > version_key("0.20261112.9"), "by number, not by text");
    }

    fn status(agent: &'static str, scope: &'static str, found: bool, current: bool) -> crate::cli::agents::Status {
        crate::cli::agents::Status {
            agent,
            scope,
            found,
            hooks: format!("/h/.{agent}/settings.json"),
            instructions: String::new(),
            after: if current { vec![Some(1)] } else { Vec::new() },
            before: if current { vec![Some(1)] } else { Vec::new() },
            strict: false,
            block: current.then_some(1),
            verdict: if current { "current" } else { "not installed" }.to_owned(),
            current,
            fix: (!current).then(|| format!("blaze agents install {agent} --{scope}")),
        }
    }

    #[test]
    fn an_agent_used_here_without_blaze_is_a_warning_once_and_an_installed_one_is_ok() {
        let checks = agents_checks(&[
            status("claude", "user", true, true),
            status("codex", "user", true, false),
            status("codex", "project", true, false),
            status("other", "user", false, false),
        ]);
        let said: Vec<(Verdict, &str)> = checks.iter().map(|it| (it.verdict, it.said.as_str())).collect();
        assert_eq!(
            said,
            [(Verdict::Ok, "claude (user scope): current"), (Verdict::Warn, "codex (user scope): not installed")],
        );
        assert_eq!(checks[1].fix.as_deref(), Some("blaze agents install codex --user"));
    }

    #[test]
    fn the_lock_says_who_holds_the_state() {
        let file = Path::new("/s/serve.lock");
        assert_eq!(lock_check(file, None).verdict, Verdict::Absent);
        assert!(lock_check(file, Some("41 stopped")).said.contains("pid 41) stopped"));
        assert!(lock_check(file, Some("41")).said.starts_with("pid 41 holds the state"));
        assert_eq!(lock_check(file, Some("forty")).verdict, Verdict::Warn);
        assert_eq!(build_check(Ok(None)).verdict, Verdict::Ok);
        let other = build_check(Ok(Some("the daemon (pid 9) is blaze /old".to_owned())));
        assert_eq!((other.verdict, other.fix.as_deref()), (Verdict::Warn, Some("blaze stop")));
    }

    #[test]
    fn a_full_disk_is_a_fault_and_old_layouts_are_named_with_their_sizes() {
        const GIB: u64 = 1 << 30;
        let state = Path::new("/r/v2");
        let used = Measured { bytes: 40 * GIB, capped: false };
        assert_eq!(disk_check(state, used, Some(500 * GIB)).verdict, Verdict::Ok);
        assert_eq!(disk_check(state, used, Some(8 * GIB)).verdict, Verdict::Warn);
        assert_eq!(disk_check(state, used, Some(GIB / 2)).verdict, Verdict::Fault);
        let root = Path::new("/r");
        assert_eq!(old_layouts_check(root, &[]).verdict, Verdict::Ok);
        let old = [(PathBuf::from("/r/runner-3"), Measured { bytes: 3 << 20, capped: true })];
        let check = old_layouts_check(root, &old);
        assert_eq!(check.verdict, Verdict::Warn);
        assert!(check.said.contains("/r/runner-3 (at least 3 MiB)"), "{check:?}");
        assert_eq!(check.fix.as_deref(), Some("rm -rf /r/runner-3"));
    }

    #[test]
    fn a_size_walk_counts_files_and_stops_at_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/x"), [0; 100]).unwrap();
        std::fs::write(dir.path().join("y"), [0; 20]).unwrap();
        let mut budget = WALK_BUDGET;
        assert_eq!(size_of(dir.path(), &mut budget), Measured { bytes: 120, capped: false });
        let mut budget = 1;
        assert!(size_of(dir.path(), &mut budget).capped);
    }

    #[test]
    fn the_toolchain_needs_its_library_sources() {
        assert_eq!(toolchain_check(Some(Path::new("/t")), true).verdict, Verdict::Ok);
        assert_eq!(toolchain_check(Some(Path::new("/t")), false).verdict, Verdict::Fault);
        assert_eq!(toolchain_check(None, false).fix.as_deref(), Some("blaze toolchain"));
        assert_eq!(shared_check(Path::new("/c/blaze"), true).verdict, Verdict::Ok);
        assert_eq!(shared_check(Path::new("/c/blaze"), false).verdict, Verdict::Absent);
    }

    #[test]
    fn a_large_untagged_directory_is_said_and_a_tagged_or_small_one_is_not() {
        let root = Path::new("/w");
        let big = Measured { bytes: 2 << 30, capped: false };
        let small = Measured { bytes: 10, capped: false };
        let sound = [("data".to_owned(), big, true), ("src".to_owned(), small, false)];
        assert_eq!(untagged_check(root, &sound).verdict, Verdict::Ok);
        let faulty = [("node_modules".to_owned(), big, false)];
        let check = untagged_check(root, &faulty);
        assert_eq!(check.verdict, Verdict::Warn);
        assert!(check.said.starts_with("node_modules/ (2048 MiB)"), "{check:?}");
        assert!(check.fix.unwrap().contains("/w/node_modules/CACHEDIR.TAG"));
    }

    #[test]
    fn whole_second_file_times_are_said() {
        let file = Path::new("/w/Cargo.toml");
        let stamp = |mtime_ns, ctime_ns| blaze_core::Stamp { ino: 1, size: 1, mtime_ns, ctime_ns, mode: 0o100644 };
        assert_eq!(stamps_check(file, stamp(1_700_000_000_123_456_789, 1_700_000_000_123_456_789)).verdict, Verdict::Ok);
        assert_eq!(stamps_check(file, stamp(1_700_000_000_000_000_000, 1_700_000_001_000_000_000)).verdict, Verdict::Warn);
    }

    #[test]
    fn no_sandbox_is_a_fault() {
        assert_eq!(sandbox_check(Ok(())).verdict, Verdict::Ok);
        let fault = sandbox_check(Err("user namespaces are off".to_owned()));
        assert_eq!(fault.verdict, Verdict::Fault);
        assert!(fault.said.ends_with("user namespaces are off"));
    }

    #[test]
    fn a_path_is_quoted_when_a_shell_would_split_it() {
        assert_eq!(quoted(Path::new("/a/b.c-d")), "/a/b.c-d");
        assert_eq!(quoted(Path::new("/a b/it's")), "'/a b/it'\\''s'");
    }
}
