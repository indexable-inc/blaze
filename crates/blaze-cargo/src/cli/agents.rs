//! `blaze agents install|hook|status|remove`: coding agents (Claude Code, Codex) told to build
//! with blaze, and shown what blaze's check says after each edit they make (docs/agents.md).
//!
//! - `install [claude|codex|all] [--project|--user] [--strict] [--dry-run]` writes into each
//!   agent's files, merged into what is there and atomically: a hook after each edit that runs
//!   `blaze agents hook <agent> vN` (Claude Code: `PostToolUse` on `Write|Edit` in
//!   `.claude/settings.json`; Codex: `PostToolUse` on `apply_patch` in `.codex/hooks.json`), a
//!   hook before each shell command that advises against cargo and rustc (refuses them under
//!   `--strict`), and a block between `<!-- blaze:agents vN begin -->` and its end marker in
//!   `CLAUDE.md` or `AGENTS.md` that says to use blaze. With no agent named, every agent whose
//!   directory (`~/.claude`, `~/.codex`) exists; with no scope, the project when run inside one,
//!   else the user's files. `--dry-run` prints the diff and writes nothing.
//! - `hook <agent> [vN] [--strict]` is what the hooks run: the agent's hook JSON on stdin, the
//!   answer on stdout (the check of the packages an edit touched, through
//!   [`Request::Diagnostics`](blaze_api::Request::Diagnostics) since the session's last token,
//!   or a word about a cargo or rustc command).
//! - `status [--json]` says what is installed where, and whether it is out of date: written by
//!   a blaze whose `vN` is below this one's ([`decide::VERSION`]).
//! - `remove [claude|codex|all] [--project|--user]` takes out exactly what install wrote: the
//!   handlers whose command is blaze's hook, and the block by its markers.
//!
//! What is decided is in [`decide`], which owns no IO; this is its driver.

use std::{
    ffi::OsString,
    fmt::Write as _,
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    time::Instant,
};

use blaze_core::exit::Exit;
use blaze_ids::{ContentId, Hasher};
use serde::Serialize;

use super::{diagnostics, reach, say};

#[blaze::sans_io]
pub mod decide;

use decide::{Agent, Checked, Event, Hooked, Owner, Scope, Verdict};

const USAGE: &str = "blaze agents install [claude|codex|all] [--project|--user] [--strict] [--dry-run]
       blaze agents status [--json]
       blaze agents remove [claude|codex|all] [--project|--user]
       blaze agents hook claude|codex [vN] [--strict]   (what the installed hooks run)";

/// `blaze agents`: its exit code.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let mut words = words.into_iter();
    let verb = words.next();
    let rest: Vec<OsString> = words.collect(); // alloc: CLI edge, the command line, once
    let done = match verb.as_ref().and_then(|it| it.to_str()) {
        Some("install") => install(rest, color),
        Some("remove") => remove(rest, color),
        Some("status") => status(rest),
        Some("hook") => return hook(rest),
        Some("--help" | "-h" | "help") => {
            blaze_report::out!("{USAGE}\n");
            return 0;
        }
        Some(other) => Err((format!("{USAGE}\n\nnot `blaze agents {other}`"), Exit::Usage.code())),
        None => Err((format!("{USAGE}\n\nwhich: install, status, remove or hook"), Exit::Usage.code())),
    };
    match done {
        Ok(code) => code,
        Err((what, code)) => {
            blaze_report::err!("{}", say::error(&what, color));
            code
        }
    }
}

type Done = Result<i32, (String, i32)>;

fn usage(what: String) -> (String, i32) {
    (format!("{USAGE}\n\n{what}"), Exit::Usage.code())
}

fn failed(what: String) -> (String, i32) {
    (what, Exit::Failed.code())
}

/// The command line of `install` and `remove`, read.
#[derive(Debug, Default, PartialEq)]
struct Line {
    /// Empty: every agent found.
    agents: Vec<Agent>,
    scope: Option<Scope>,
    strict: bool,
    dry_run: bool,
    json: bool,
}

fn read(words: Vec<OsString>, verb: &str, takes: &[&str]) -> Result<Line, (String, i32)> {
    let mut line = Line::default();
    for word in words {
        let Some(word) = word.to_str() else {
            return Err(usage(format!("{} is not UTF-8", word.to_string_lossy())));
        };
        let flag = word.starts_with('-');
        if flag && !takes.contains(&word) {
            return Err(usage(format!("`blaze agents {verb}` takes no `{word}`")));
        }
        match word {
            "--project" => line.scope = Some(Scope::Project),
            "--user" => line.scope = Some(Scope::User),
            "--strict" => line.strict = true,
            "--dry-run" => line.dry_run = true,
            "--json" => line.json = true,
            "all" => line.agents = Agent::ALL.to_vec(),
            _ => match Agent::named(word) {
                Some(agent) if !line.agents.contains(&agent) => line.agents.push(agent),
                Some(_) => {}
                None => return Err(usage(format!("no agent `{word}`: claude, codex or all"))),
            },
        }
    }
    Ok(line)
}

/// Where the agents' files are looked for: the home directory, and the project run in.
struct Places {
    home: PathBuf,
    /// The workspace root of the Cargo project the command runs in, if it runs in one.
    project: Option<PathBuf>,
}

fn places() -> Result<Places, String> {
    let home = std::env::var_os("HOME")
        .filter(|it| !it.is_empty())
        .map(PathBuf::from)
        .ok_or("HOME is not set: blaze agents finds ~/.claude and ~/.codex under it")?;
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    Ok(Places { home, project: project_of(&cwd)? })
}

/// The workspace root of the Cargo project `dir` is in: the nearest directory at or above the
/// nearest Cargo.toml whose Cargo.toml has `[workspace]`, else that Cargo.toml's directory.
pub(super) fn project_of(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut nearest = None;
    for at in dir.ancestors() {
        let manifest = at.join("Cargo.toml");
        let Some(text) = read_optional(&manifest)? else { continue };
        nearest.get_or_insert_with(|| at.to_path_buf());
        if toml::from_str::<toml::Table>(&text).is_ok_and(|it| it.contains_key("workspace")) {
            return Ok(Some(at.to_path_buf()));
        }
    }
    Ok(nearest)
}

/// The text of `path`, `None` when there is no such file; any other failure names it.
fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// `text` as the whole of `path`, atomically: written to a file next to it, synced, given the
/// old file's permissions, renamed over it, and the directory synced. Its directories are made.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let named = |what: &str, error: io::Error| format!("{what} {}: {error}", path.display());
    let dir = path.parent().ok_or_else(|| format!("{} has no directory", path.display()))?;
    fs::create_dir_all(dir).map_err(|error| named("making the directory of", error))?;
    let name = path.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
    let temporary = dir.join(format!(".{name}.blaze-{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(text.as_bytes())?;
        if let Ok(old) = fs::metadata(path) {
            file.set_permissions(old.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if let Err(error) = written {
        _ = fs::remove_file(&temporary);
        return Err(named("writing", error));
    }
    Ok(())
}

/// The agents a command acts on: those named, else every agent whose directory exists in the
/// home directory.
fn chosen(line: &Line, places: &Places) -> Result<Vec<Agent>, String> {
    if !line.agents.is_empty() {
        return Ok(line.agents.clone());
    }
    let found: Vec<Agent> =
        Agent::ALL.into_iter().filter(|it| places.home.join(it.dir()).is_dir()).collect();
    match found.is_empty() {
        true => Err(format!(
            "no agent found: neither {} nor {} exists; name one: `blaze agents install claude`",
            places.home.join(".claude").display(),
            places.home.join(".codex").display()
        )),
        false => Ok(found),
    }
}

/// The scope a command acts at, and the directory its files are under.
fn base(line: &Line, places: &Places) -> Result<(Scope, PathBuf), String> {
    match (line.scope, &places.project) {
        (Some(Scope::User), _) | (None, None) => Ok((Scope::User, places.home.clone())),
        (Some(Scope::Project) | None, Some(project)) => Ok((Scope::Project, project.clone())),
        (Some(Scope::Project), None) => {
            Err("--project: this is run in no Cargo project (no Cargo.toml here or above)".to_owned())
        }
    }
}

/// One file a command changes: its text before (`None`: none) and after (`None`: removed).
struct Change {
    path: PathBuf,
    old: Option<String>,
    new: Option<String>,
    what: &'static str,
}

/// The changes made, or under `dry_run` their diffs printed: every refusal comes before any
/// write, so a file that cannot be merged leaves every file as it was.
fn apply(changes: &[Change], dry_run: bool) -> Result<String, String> {
    let mut said = String::new(); // alloc: CLI edge, what is said, once
    for change in changes {
        let name = change.path.display().to_string();
        if change.old == change.new {
            let _ = writeln!(said, "{name}: {} already current", change.what);
            continue;
        }
        if dry_run {
            let old = change.old.as_deref().unwrap_or_default();
            said.push_str(&decide::diff(&name, old, change.new.as_deref().unwrap_or_default()));
            continue;
        }
        match &change.new {
            Some(new) => write_atomic(&change.path, new)?,
            None => fs::remove_file(&change.path)
                .map_err(|error| format!("removing {name}, left empty: {error}"))?,
        }
        let verb = match (&change.old, &change.new) {
            (None, _) => "wrote",
            (_, None) => "removed",
            _ => "updated",
        };
        let _ = writeln!(said, "{name}: {verb} {}", change.what);
    }
    Ok(said)
}

fn install(words: Vec<OsString>, _color: bool) -> Done {
    let line = read(words, "install", &["--project", "--user", "--strict", "--dry-run"])?;
    let places = places().map_err(failed)?;
    let agents = chosen(&line, &places).map_err(failed)?;
    let (scope, base) = base(&line, &places).map_err(failed)?;
    let mut changes = Vec::new(); // alloc: CLI edge, once
    for agent in &agents {
        let files = decide::files(*agent, scope, &base);
        let old = read_optional(&files.hooks).map_err(failed)?;
        let name = files.hooks.display().to_string();
        let new = decide::merged(&name, old.as_deref(), &decide::ours(*agent, line.strict)).map_err(failed)?;
        changes.push(Change { path: files.hooks, old, new: Some(new), what: "blaze's hooks" });
        let old = read_optional(&files.instructions).map_err(failed)?;
        let name = files.instructions.display().to_string();
        let new = decide::with_block(&name, old.as_deref()).map_err(failed)?;
        changes.push(Change { path: files.instructions, old, new: Some(new), what: "blaze's block" });
    }
    let mut said = apply(&changes, line.dry_run).map_err(failed)?;
    if agents.contains(&Agent::Codex) {
        said.push_str(
            "codex: Codex runs a hook only once it is trusted: open `/hooks` in Codex and trust blaze's",
        );
        if scope == Scope::Project {
            said.push_str(", and trust the project's .codex/ layer");
        }
        said.push('\n');
    }
    if !on_path("blaze") {
        said.push_str(
            "warning: the hooks run `blaze` from PATH, and no `blaze` is on this one: put blaze on the PATH the agents run with\n",
        );
    }
    if line.dry_run {
        said.push_str("--dry-run: nothing was written\n");
    }
    blaze_report::out!("{said}");
    Ok(0)
}

fn remove(words: Vec<OsString>, _color: bool) -> Done {
    let line = read(words, "remove", &["--project", "--user", "--dry-run"])?;
    let places = places().map_err(failed)?;
    let agents = match line.agents.is_empty() {
        true => Agent::ALL.to_vec(),
        false => line.agents.clone(),
    };
    let (scope, base) = base(&line, &places).map_err(failed)?;
    let mut changes = Vec::new(); // alloc: CLI edge, once
    for agent in agents {
        let files = decide::files(agent, scope, &base);
        if let Some(old) = read_optional(&files.hooks).map_err(failed)? {
            let name = files.hooks.display().to_string();
            if let Some(new) = decide::unmerged(&name, &old).map_err(failed)? {
                changes.push(Change { path: files.hooks, old: Some(old), new: Some(new), what: "blaze's hooks" });
            }
        }
        if let Some(old) = read_optional(&files.instructions).map_err(failed)? {
            let name = files.instructions.display().to_string();
            if let Some(new) = decide::without_block(&name, &old).map_err(failed)? {
                let new = Some(new).filter(|it| !it.trim().is_empty());
                changes.push(Change { path: files.instructions, old: Some(old), new, what: "blaze's block" });
            }
        }
    }
    let mut said = apply(&changes, line.dry_run).map_err(failed)?;
    if changes.is_empty() {
        let _ = writeln!(said, "nothing of blaze's is installed for the {}", scope.name());
    }
    blaze_report::out!("{said}");
    Ok(0)
}

/// Whether `program` is an executable file in a directory of PATH.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let file = dir.join(program);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(&file).is_ok_and(|it| it.is_file() && it.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            file.is_file()
        }
    })
}

/// What is installed for one agent at one scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub agent: &'static str,
    pub scope: &'static str,
    /// The agent's directory exists in the home directory: the agent is used here.
    pub found: bool,
    pub hooks: String,
    pub instructions: String,
    /// The `vN` of each hook of blaze's found (`null`: the plugin's, unversioned), by event.
    pub after: Vec<Option<u32>>,
    pub before: Vec<Option<u32>>,
    pub strict: bool,
    /// The block's `vN`; `null`: no block.
    pub block: Option<u32>,
    /// `current`, `out of date (...)`, `partly installed`, `not installed`, or the file that
    /// could not be read and why.
    pub verdict: String,
    /// The verdict is that it is all there and of this blaze's version.
    pub current: bool,
    /// The command that makes it current; `null` when it is.
    pub fix: Option<String>,
}

/// What is installed for every agent, at the user's scope and the project's (when run in
/// one).
pub fn statuses() -> Result<Vec<Status>, String> {
    let places = places()?;
    let mut found = Vec::new(); // alloc: CLI edge, once
    let scopes = [Some((Scope::User, places.home.clone())), places.project.clone().map(|it| (Scope::Project, it))];
    for (scope, base) in scopes.into_iter().flatten() {
        for agent in Agent::ALL {
            found.push(status_of(agent, scope, &base, places.home.join(agent.dir()).is_dir()));
        }
    }
    Ok(found)
}

fn status_of(agent: Agent, scope: Scope, base: &Path, found: bool) -> Status {
    let files = decide::files(agent, scope, base);
    let (hooks, instructions) = (files.hooks.display().to_string(), files.instructions.display().to_string());
    let read = (|| -> Result<(Vec<Hooked>, Option<u32>), String> {
        let hooked = match read_optional(&files.hooks)? {
            Some(text) => decide::hooked(&hooks, &text)?,
            None => Vec::new(),
        };
        let block = match read_optional(&files.instructions)? {
            Some(text) => decide::block_version(&instructions, &text)?,
            None => None,
        };
        Ok((hooked, block))
    })();
    let (hooked, block, verdict) = match read {
        Ok((hooked, block)) => {
            let verdict = Verdict::of(&hooked, block);
            (hooked, block, Ok(verdict))
        }
        Err(why) => (Vec::new(), None, Err(why)),
    };
    let by = |event: &str| -> Vec<Option<u32>> {
        hooked.iter().filter(|it| it.event == event).map(|it| it.version).collect()
    };
    let current = verdict == Ok(Verdict::Current);
    let fix = match &verdict {
        Ok(Verdict::Current) => None,
        Ok(Verdict::Newer(_)) => Some("update blaze: a newer one wrote this".to_owned()),
        Ok(_) => Some(format!("blaze agents install {} --{}", agent.name(), scope.name())),
        Err(_) => Some(format!("fix the file, then `blaze agents install {} --{}`", agent.name(), scope.name())),
    };
    Status {
        agent: agent.name(),
        scope: scope.name(),
        found,
        after: by("PostToolUse"),
        before: by("PreToolUse"),
        strict: hooked.iter().any(|it| it.strict),
        block,
        verdict: match verdict {
            Ok(verdict) => verdict.word(),
            Err(why) => why,
        },
        current,
        fix,
        hooks,
        instructions,
    }
}

fn status(words: Vec<OsString>) -> Done {
    let line = read(words, "status", &["--json"])?;
    let statuses = statuses().map_err(failed)?;
    let text = match line.json {
        true => format!("{}\n", serde_json::json!({ "version": decide::VERSION, "agents": statuses })),
        false => {
            let mut text = String::new(); // alloc: CLI edge, once
            for it in &statuses {
                let used = if it.found { "" } else { " (agent not found here)" };
                let _ = writeln!(text, "{} {}{used}: {}", it.agent, it.scope, it.verdict);
                let _ = writeln!(text, "  hooks: {}", it.hooks);
                let _ = writeln!(text, "  instructions: {}", it.instructions);
                if let Some(fix) = &it.fix {
                    let _ = writeln!(text, "  fix: {fix}");
                }
            }
            text
        }
    };
    blaze_report::out!("{text}");
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// The hook.

/// `blaze agents hook <agent> [vN] [--strict]`: the agent's hook JSON on stdin, the answer on
/// stdout; always exit 0, since the answer, blaze's own failure too, is in the JSON the agent
/// reads.
fn hook(words: Vec<OsString>) -> i32 {
    let words: Vec<String> = words.into_iter().map(|it| it.to_string_lossy().into_owned()).collect();
    let agent = words.first().and_then(|it| Agent::named(it));
    let strict = words.iter().any(|it| it == "--strict");
    let answer = match agent {
        None => Some(decide::hook_failed(
            "PostToolUse",
            &format!("`blaze agents hook {}`: name the agent, claude or codex", words.join(" ")),
        )),
        Some(agent) => hooked_answer(agent, strict).unwrap_or_else(|what| Some(decide::hook_failed("PostToolUse", &what))),
    };
    if let Some(answer) = answer {
        blaze_report::out!("{answer}\n");
    }
    0
}

fn hooked_answer(agent: Agent, strict: bool) -> Result<Option<serde_json::Value>, String> {
    let mut text = String::new(); // alloc: the hook's input, once
    io::stdin().read_to_string(&mut text).map_err(|error| format!("reading the hook input: {error}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("the hook input is not JSON: {error}"))?;
    let input = decide::input(agent, &value)?;
    match input.event {
        Event::Before => Ok(input
            .command
            .as_deref()
            .and_then(decide::cargo_in)
            .map(|found| decide::before_answer(&found, strict))),
        Event::After => Ok(after(agent, &input)),
        Event::Other(_) => Ok(None),
    }
}

/// The directory the session's tokens are kept in: under the agent's scratchpad for the
/// session when it names one, else the system's temporary directory; a directory per session.
fn session_dir(input: &decide::Input) -> PathBuf {
    let root = match &input.scratch {
        Some(scratch) => PathBuf::from(scratch),
        None => std::env::temp_dir(),
    };
    let session = Hasher::new("blaze agents session 1").part(input.session.as_bytes()).finish();
    root.join("blaze-agents").join(&format!("{session:x}")[..24])
}

/// The file of the token of `owner`'s last check in the session.
fn token_file(dir: &Path, owner: &Owner) -> PathBuf {
    let id = Hasher::new("blaze agents token 1").part(owner.manifest.as_os_str().as_encoded_bytes()).finish();
    dir.join(format!("{}.token", &format!("{id:x}")[..24]))
}

/// The token in `file`; `None` when there is none. One that does not read is said, so the
/// check shown as a first one is not a quiet reset.
fn read_token(file: &Path) -> Result<Option<ContentId>, String> {
    match read_optional(file)? {
        None => Ok(None),
        Some(text) => reach::root_of(text.trim()).map(Some).ok_or_else(|| {
            format!("the session's last token {} is not 64 hex digits: shown as a first check", file.display())
        }),
    }
}

/// The checks of the packages `input`'s edit touched, within [`decide::DEADLINE_MS`], as the
/// hook answers them.
fn after(agent: Agent, input: &decide::Input) -> Option<serde_json::Value> {
    let started = Instant::now();
    let cwd = PathBuf::from(&input.cwd);
    let mut notes: Vec<String> = input.unread.iter().cloned().collect(); // alloc: once a hook
    let mut owners: Vec<(Owner, Vec<PathBuf>)> = Vec::new(); // alloc: as above
    for file in &input.files {
        let path = cwd.join(file);
        if !decide::checked(&path) {
            continue;
        }
        let mut manifests = Vec::new(); // alloc: as above
        for dir in path.ancestors().skip(1) {
            let manifest = dir.join("Cargo.toml");
            match read_optional(&manifest) {
                Ok(None) => {}
                Ok(Some(text)) => manifests.push((manifest, Ok(text))),
                Err(why) => manifests.push((manifest, Err(why))),
            }
        }
        let Some(owner) = decide::owner_of(&manifests) else { continue };
        // An edit of many files of one package is one check of it.
        match owners.iter_mut().find(|(it, _)| *it == owner) {
            Some((_, files)) => files.push(path),
            None => owners.push((owner, vec![path])),
        }
    }
    let dir = session_dir(input);
    let client = format!("{}:{}", agent.name(), input.session);
    let mut results = Vec::new(); // alloc: as above
    for (owner, files) in owners {
        let spent = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let left = decide::DEADLINE_MS.saturating_sub(spent);
        if left == 0 {
            results.push(Checked::Late { owner });
            continue;
        }
        let file = token_file(&dir, &owner);
        let since = read_token(&file).unwrap_or_else(|note| {
            notes.push(note);
            None
        });
        // The check runs from the manifest's directory, so every hook of the session asks the
        // same targets (the targets' key has the directory cargo ran in).
        if let Some(home) = owner.manifest.parent()
            && let Err(error) = std::env::set_current_dir(home)
        {
            let what = format!("changing to {}: {error}", home.display());
            results.push(Checked::Failed { owner, what });
            continue;
        }
        let words = owner.check().into_iter().map(OsString::from).collect();
        match diagnostics::ask(words, since, &client, left, false) {
            Ok((seen, head)) => {
                if let Some(token) = seen.token
                    && Some(token) != since
                    && let Err(why) = write_atomic(&file, &format!("{token:x}\n"))
                {
                    notes.push(format!("the session's token could not be kept, so the next check is a first one: {why}"));
                }
                let head = Path::new(&head);
                let edited = files.iter().filter_map(|it| diagnostics::in_checkout(&cwd, it, head)).collect();
                let cwd = diagnostics::in_checkout(&cwd, Path::new(""), head);
                results.push(Checked::Seen { owner, seen, edited, cwd });
            }
            Err(what) => results.push(Checked::Failed { owner, what }),
        }
    }
    decide::after_answer(agent, &results, &notes)
}
