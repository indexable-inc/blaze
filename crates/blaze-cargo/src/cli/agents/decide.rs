//! What `blaze agents` decides, owning no IO: what install writes into an agent's files and
//! remove takes out, what status reads from them, which package an edit is of, and what the
//! hook answers an agent. The driver (`super`) reads and writes the files, asks the daemon and
//! keeps the session's tokens.
//!
//! What install writes is versioned by [`VERSION`]: the hook's command line ends `vN` and the
//! instructions block's markers say `vN`; one written by an older blaze is out of date.

use std::path::{Path, PathBuf};

use blaze_api::{Base, Found, Level, Seen};
use serde_json::{Map, Value, json};

use crate::cli::diagnostics;

/// The version of what `blaze agents install` writes: raised whenever the hooks or the block
/// change, so `status` and `blaze doctor` say an older install is out of date.
pub const VERSION: u32 = 1;
/// The hook handler's timeout, in seconds: over [`DEADLINE_MS`], so the hook says "still
/// running" itself rather than being killed by the agent.
pub const HOOK_TIMEOUT_S: u64 = 30;
/// How long the hook waits for the daemon's check of every package an edit touched.
pub const DEADLINE_MS: u64 = 20_000;
/// At most this many diagnostics of a kind are drawn ...
pub const CAP: usize = 10;
/// ... within this many characters (under Codex's ~2,500-token cap on a hook's message).
pub const TEXT_BUDGET: usize = 8000;
/// The last lines of blaze's own failure that are said.
pub const TAIL_LINES: usize = 15;
/// The words every hook command of blaze's starts with: what makes a handler blaze's.
pub const HOOK_WORDS: &str = "blaze agents hook";

/// A coding agent `blaze agents` knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn named(word: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|it| it.name() == word)
    }

    /// Its configuration directory, under the home directory or a project.
    pub fn dir(self) -> &'static str {
        match self {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
        }
    }

    /// The tool names its edits come as: what the after-edit hook matches.
    pub fn edit_matcher(self) -> &'static str {
        match self {
            // plugins/claude/FORMAT-NOTES.md says why not `MultiEdit` or `NotebookEdit`.
            Agent::Claude => "Write|Edit",
            Agent::Codex => "apply_patch",
        }
    }

    /// What the edit is called in the hook's blocking reason.
    fn edit_word(self) -> &'static str {
        match self {
            Agent::Claude => "The edit",
            Agent::Codex => "The patch",
        }
    }
}

/// Where install writes: a project's files, or the user's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }
}

/// The two files of one agent at one scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Files {
    /// The hooks: Claude Code's `settings.json`, Codex's `hooks.json`.
    pub hooks: PathBuf,
    /// The instructions: `CLAUDE.md` or `AGENTS.md`.
    pub instructions: PathBuf,
}

/// `agent`'s files at `scope`, `base` the project's root or the home directory.
pub fn files(agent: Agent, scope: Scope, base: &Path) -> Files {
    let dir = base.join(agent.dir());
    let hooks = match agent {
        Agent::Claude => dir.join("settings.json"),
        Agent::Codex => dir.join("hooks.json"),
    };
    let name = match agent {
        Agent::Claude => "CLAUDE.md",
        Agent::Codex => "AGENTS.md",
    };
    let instructions = match scope {
        Scope::Project => base.join(name),
        Scope::User => dir.join(name),
    };
    Files { hooks, instructions }
}

/// The hook command line of `agent`: `blaze agents hook claude v1`, `--strict` added when the
/// before-command hook refuses cargo and rustc rather than advising against them.
pub fn command(agent: Agent, strict: bool) -> String {
    let strict = if strict { " --strict" } else { "" };
    format!("{HOOK_WORDS} {} v{VERSION}{strict}", agent.name()) // alloc: a command line, once
}

/// One hook install puts in: under `event`, in the group of `matcher`.
#[derive(Clone, PartialEq, Debug)]
pub struct Ours {
    pub event: &'static str,
    pub matcher: &'static str,
    pub handler: Value,
}

/// What install puts in `agent`'s hooks file: the check after each edit, and the word about
/// cargo and rustc before each shell command.
pub fn ours(agent: Agent, strict: bool) -> [Ours; 2] {
    let after = match agent {
        Agent::Claude => json!({
            "type": "command",
            "command": command(agent, false),
            "timeout": HOOK_TIMEOUT_S,
        }),
        Agent::Codex => json!({
            "type": "command",
            "command": command(agent, false),
            "timeout": HOOK_TIMEOUT_S,
            "statusMessage": "blaze check",
        }),
    };
    let before = json!({ "type": "command", "command": command(agent, strict), "timeout": 5 });
    [
        Ours { event: "PostToolUse", matcher: agent.edit_matcher(), handler: after },
        Ours { event: "PreToolUse", matcher: "Bash", handler: before },
    ]
}

/// Whether `handler` is one of blaze's: its command starts with [`HOOK_WORDS`].
fn is_ours(handler: &Value) -> bool {
    handler.get("command").and_then(Value::as_str).is_some_and(|it| {
        it == HOOK_WORDS || it.strip_prefix(HOOK_WORDS).is_some_and(|rest| rest.starts_with(' '))
    })
}

/// The hooks file's text read as its top object, refusing what is not the shape the agents
/// read: the error names the file and the key.
fn top(file: &str, text: Option<&str>) -> Result<Map<String, Value>, String> {
    let Some(text) = text.filter(|it| !it.trim().is_empty()) else {
        return Ok(Map::new());
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(top)) => Ok(top),
        Ok(_) => Err(format!("{file} is not a JSON object: blaze writes its hooks into one")),
        Err(error) => Err(format!("{file} does not read as JSON ({error}): fix it, then install again")),
    }
}

/// The `hooks` object of `top`, each event's list of groups checked.
fn events<'a>(file: &str, top: &'a mut Map<String, Value>) -> Result<&'a mut Map<String, Value>, String> {
    let hooks = top.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(format!("{file}: `hooks` is not an object"));
    };
    for (event, groups) in hooks.iter() {
        let Value::Array(groups) = groups else {
            return Err(format!("{file}: `hooks.{event}` is not a list"));
        };
        for (at, group) in groups.iter().enumerate() {
            match group.get("hooks") {
                Some(Value::Array(_)) if group.is_object() => {}
                _ => return Err(format!("{file}: `hooks.{event}[{at}].hooks` is not a list")),
            }
        }
    }
    Ok(hooks)
}

fn handlers(group: &mut Value) -> &mut Vec<Value> {
    match group.get_mut("hooks") {
        Some(Value::Array(handlers)) => handlers,
        _ => unreachable!("`events` checked every group's `hooks` is a list"),
    }
}

/// The text of the hooks file `file` (`None`: there is none) with `ours` in it: each put in
/// place of blaze's handler in the group of its matcher, else at the end of that group, else
/// in a new group; any other handler of blaze's under the same event taken out. What is not
/// blaze's is kept as it is, in its order. Installing twice writes the same text.
pub fn merged(file: &str, text: Option<&str>, ours: &[Ours]) -> Result<String, String> {
    let mut top = top(file, text)?;
    let hooks = events(file, &mut top)?;
    for one in ours {
        let groups = hooks.entry(one.event).or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else { unreachable!("`events` checked it") };
        let home = groups.iter().position(|it| it.get("matcher").and_then(Value::as_str) == Some(one.matcher));
        // Blaze's handlers under this event outside the group it belongs in go.
        for (at, group) in groups.iter_mut().enumerate() {
            if Some(at) != home {
                handlers(group).retain(|it| !is_ours(it));
            }
        }
        match home {
            Some(at) => {
                let list = handlers(&mut groups[at]);
                match list.iter().position(is_ours) {
                    Some(mine) => {
                        list[mine] = one.handler.clone(); // clone: the handler, put in once
                        let mut seen = false;
                        list.retain(|it| !is_ours(it) || !std::mem::replace(&mut seen, true));
                    }
                    None => list.push(one.handler.clone()), // clone: as above
                }
            }
            // alloc: a new group, once
            None => groups.push(json!({ "matcher": one.matcher, "hooks": [one.handler.clone()] })),
        }
        groups.retain(|it| it.get("hooks").and_then(Value::as_array).is_some_and(|it| !it.is_empty()));
    }
    Ok(pretty(&Value::Object(top)))
}

/// The text of `file` with every handler of blaze's taken out, a group or event left empty by
/// that taken out too; `None` when it has none.
pub fn unmerged(file: &str, text: &str) -> Result<Option<String>, String> {
    let mut top = top(file, Some(text))?;
    if !top.contains_key("hooks") {
        return Ok(None);
    }
    let hooks = events(file, &mut top)?;
    let mut found = false;
    for groups in hooks.values_mut() {
        let Value::Array(groups) = groups else { unreachable!("`events` checked it") };
        for group in groups.iter_mut() {
            let list = handlers(group);
            let before = list.len();
            list.retain(|it| !is_ours(it));
            found |= list.len() != before;
        }
        groups.retain(|it| it.get("hooks").and_then(Value::as_array).is_some_and(|it| !it.is_empty()));
    }
    if !found {
        return Ok(None);
    }
    hooks.retain(|_, groups| groups.as_array().is_some_and(|it| !it.is_empty()));
    if hooks.is_empty() {
        top.shift_remove("hooks");
    }
    Ok(Some(pretty(&Value::Object(top))))
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a JSON value always writes");
    text.push('\n');
    text
}

/// A hook of blaze's an agent's hooks file has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hooked {
    pub event: String,
    pub matcher: Option<String>,
    /// The `vN` its command ends with; `None` for one with none (the plugin's, which follows
    /// the blaze on PATH).
    pub version: Option<u32>,
    pub strict: bool,
}

/// The hooks of blaze's in the hooks file `file`.
pub fn hooked(file: &str, text: &str) -> Result<Vec<Hooked>, String> {
    let mut top = top(file, Some(text))?;
    if !top.contains_key("hooks") {
        return Ok(Vec::new());
    }
    let hooks = events(file, &mut top)?;
    let mut found = Vec::new(); // alloc: a status, once
    for (event, groups) in hooks.iter() {
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group.get("matcher").and_then(Value::as_str).map(str::to_owned);
            for handler in group.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if !is_ours(handler) {
                    continue;
                }
                let words = handler.get("command").and_then(Value::as_str).unwrap_or_default();
                let version = words.split_whitespace().find_map(|it| it.strip_prefix('v')?.parse().ok());
                let strict = words.split_whitespace().any(|it| it == "--strict");
                // clone: the status's own names, once
                found.push(Hooked { event: event.clone(), matcher: matcher.clone(), version, strict });
            }
        }
    }
    Ok(found)
}

const BEGIN: &str = "<!-- blaze:agents v";
const BEGIN_END: &str = " begin -->";
const END_END: &str = " end -->";

/// The managed block install writes into an agent's instructions file.
pub fn block() -> String {
    // alloc: the block, once
    format!(
        "{BEGIN}{VERSION}{BEGIN_END}
## Building Rust here: blaze

This is built with blaze, which is both cargo and rustc. `blaze agents install` wrote this
block and `blaze agents remove` takes it out; text outside its markers is never touched.

- Build, check, test and run with `blaze build`, `blaze check`, `blaze test` and `blaze run`:
  they take cargo's flags. Do not run `cargo` or `rustc`; blaze's daemon already has every edit.
- After each edit, blaze checks the package the file is in and says what is new: new errors
  block until they are fixed, warnings and counts come as context. `blaze diagnostics` says the
  same on demand.
- Why a build was slow: `blaze slow`. Why something was computed again: `blaze why`.
- Something wrong with blaze itself: `blaze doctor`.
{BEGIN}{VERSION}{END_END}
"
    )
}

/// Where the block is in `text`: its start, its end (past its end marker's line break) and
/// its version; `None` when there is none. A begin marker with no end is refused, naming the
/// file.
fn find_block(file: &str, text: &str) -> Result<Option<(usize, usize, u32)>, String> {
    let mut from = 0;
    while let Some(at) = text[from..].find(BEGIN).map(|it| it + from) {
        let rest = &text[at + BEGIN.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(version) = rest[..digits].parse::<u32>() else {
            from = at + BEGIN.len();
            continue;
        };
        if !rest[digits..].starts_with(BEGIN_END) {
            from = at + BEGIN.len();
            continue;
        }
        let body = at + BEGIN.len() + digits + BEGIN_END.len();
        let mut look = body;
        while let Some(end) = text[look..].find(BEGIN).map(|it| it + look) {
            let rest = &text[end + BEGIN.len()..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && rest[digits..].starts_with(END_END) {
                let mut stop = end + BEGIN.len() + digits + END_END.len();
                if text[stop..].starts_with('\n') {
                    stop += 1;
                }
                return Ok(Some((at, stop, version)));
            }
            look = end + BEGIN.len();
        }
        return Err(format!(
            "{file} has blaze's begin marker `{BEGIN}{version}{BEGIN_END}` and no end marker after it: \
             put back the end or take out the begin, then run blaze agents again"
        ));
    }
    Ok(None)
}

/// The version of the block in `text`, `None` when it has none.
pub fn block_version(file: &str, text: &str) -> Result<Option<u32>, String> {
    Ok(find_block(file, text)?.map(|(_, _, version)| version))
}

/// `text` (`None`: no file) with this blaze's block: put in place of the one there, else at its
/// end after a blank line.
pub fn with_block(file: &str, text: Option<&str>) -> Result<String, String> {
    let text = text.unwrap_or_default();
    let block = block();
    match find_block(file, text)? {
        Some((start, stop, _)) => Ok(format!("{}{block}{}", &text[..start], &text[stop..])), // alloc: once
        None if text.is_empty() => Ok(block),
        None => {
            let gap = if text.ends_with("\n\n") {
                ""
            } else if text.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            Ok(format!("{text}{gap}{block}")) // alloc: once
        }
    }
}

/// `text` without the block, and the blank line install put before it; `None` when it has no
/// block. An empty text after is the file's end: the driver removes the file.
pub fn without_block(file: &str, text: &str) -> Result<Option<String>, String> {
    let Some((start, stop, _)) = find_block(file, text)? else {
        return Ok(None);
    };
    let mut before = &text[..start];
    if stop == text.len() && before.ends_with("\n\n") {
        before = &before[..before.len() - 1];
    }
    Ok(Some(format!("{before}{}", &text[stop..]))) // alloc: once
}

/// How current what install wrote is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Both hooks and the block, of this blaze's version (or the plugin's unversioned hook).
    Current,
    /// Something of an older version: `blaze agents install` brings it up to date.
    Old(u32),
    /// Some of it and not the rest.
    Partial,
    /// None of it.
    Missing,
    /// Something of a newer blaze than this one.
    Newer(u32),
}

impl Verdict {
    /// The verdict of the hooks found and the block's version.
    pub fn of(hooked: &[Hooked], block: Option<u32>) -> Verdict {
        let after = hooked.iter().any(|it| it.event == "PostToolUse");
        let before = hooked.iter().any(|it| it.event == "PreToolUse");
        if !after && !before && block.is_none() {
            return Verdict::Missing;
        }
        let versions = hooked.iter().filter_map(|it| it.version).chain(block);
        if let Some(newer) = versions.clone().filter(|it| *it > VERSION).max() {
            return Verdict::Newer(newer);
        }
        if let Some(old) = versions.filter(|it| *it < VERSION).min() {
            return Verdict::Old(old);
        }
        match after && before && block.is_some() {
            true => Verdict::Current,
            false => Verdict::Partial,
        }
    }

    pub fn word(self) -> String {
        match self {
            Verdict::Current => "current".to_owned(),
            Verdict::Old(version) => format!("out of date (v{version}, this blaze writes v{VERSION})"),
            Verdict::Partial => "partly installed".to_owned(),
            Verdict::Missing => "not installed".to_owned(),
            Verdict::Newer(version) => format!("written by a newer blaze (v{version}; this one writes v{VERSION})"),
        }
    }
}

/// A unified diff of `old` to `new`, line by line, for `--dry-run`: a longest common
/// subsequence, with three lines of context.
pub fn diff(name: &str, old: &str, new: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let (n, m) = (a.len(), b.len());
    // alloc: the table of a dry run's diff, once a file
    let mut longest = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            longest[i * (m + 1) + j] = match a[i] == b[j] {
                true => longest[(i + 1) * (m + 1) + j + 1] + 1,
                false => longest[(i + 1) * (m + 1) + j].max(longest[i * (m + 1) + j + 1]),
            };
        }
    }
    // Each line: ' ', '-' or '+', with its text.
    let mut lines: Vec<(char, &str)> = Vec::new(); // alloc: as above
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            lines.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < n && (j == m || longest[(i + 1) * (m + 1) + j] >= longest[i * (m + 1) + j + 1]) {
            lines.push(('-', a[i]));
            i += 1;
        } else {
            lines.push(('+', b[j]));
            j += 1;
        }
    }
    let mut out = format!("--- {name}\n+++ {name} (after)\n"); // alloc: as above
    let changed: Vec<usize> = (0..lines.len()).filter(|it| lines[*it].0 != ' ').collect(); // alloc: as above
    let mut at = 0;
    while at < changed.len() {
        let start = changed[at].saturating_sub(3);
        let mut stop = changed[at] + 1;
        while at + 1 < changed.len() && changed[at + 1] <= stop + 6 {
            at += 1;
            stop = changed[at] + 1;
        }
        let stop = (stop + 3).min(lines.len());
        out.push_str("@@\n");
        for (mark, text) in &lines[start..stop] {
            out.push(*mark);
            out.push_str(text);
            out.push('\n');
        }
        at += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The hook.

/// The event a hook was run for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// After a tool ran: an edit, checked.
    After,
    /// Before a tool runs: a shell command, looked at for cargo and rustc.
    Before,
    Other(String),
}

/// What an agent's hook input says, read leniently: agents add fields to it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Input {
    pub event: Event,
    pub session: String,
    pub cwd: String,
    pub tool: String,
    /// The files the tool wrote, each once, as the input names them.
    pub files: Vec<String>,
    /// Why no file could be read from a write's input, when none could.
    pub unread: Option<String>,
    /// A shell command's text, before it runs.
    pub command: Option<String>,
    /// A directory of the session's the agent names (Claude Code's scratchpad), to keep the
    /// session's tokens in.
    pub scratch: Option<String>,
}

/// The file names an `apply_patch` patch writes: its `*** Add File:`, `*** Update File:`,
/// `*** Delete File:` and `*** Move to:` lines.
fn patch_files(patch: &str, out: &mut Vec<String>) {
    for line in patch.lines() {
        let named = ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "]
            .into_iter()
            .find_map(|it| line.strip_prefix(it));
        if let Some(name) = named.map(str::trim_end).filter(|it| !it.is_empty()) {
            out.push(name.to_owned()); // alloc: a file name, once an edit
        }
    }
}

/// `value`, the hook's JSON, read as `agent`'s.
pub fn input(agent: Agent, value: &Value) -> Result<Input, String> {
    let Value::Object(top) = value else {
        return Err("the hook input is not a JSON object".to_owned());
    };
    let text = |key: &str| top.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(session), Some(cwd)) = (text("session_id"), text("cwd")) else {
        return Err("the hook input has no `session_id` or `cwd`".to_owned());
    };
    let event = match text("hook_event_name").as_deref() {
        Some("PostToolUse") | None => Event::After,
        Some("PreToolUse") => Event::Before,
        Some(other) => Event::Other(other.to_owned()),
    };
    let tool = text("tool_name").unwrap_or_default();
    let tool_input = top.get("tool_input");
    let field = |key: &str| tool_input.and_then(|it| it.get(key)).and_then(Value::as_str);
    let mut files = Vec::new(); // alloc: an edit's files, once
    let mut unread = None;
    let mut command = None;
    match event {
        Event::Before => command = field("command").map(str::to_owned),
        _ => {
            if let Some(file) = field("file_path").or_else(|| field("notebook_path")) {
                files.push(file.to_owned());
            } else if let Some(patch) = field("command").filter(|_| agent == Agent::Codex) {
                patch_files(patch, &mut files);
                if files.is_empty() {
                    unread = Some(format!(
                        "the blaze hook found no `*** ... File:` line in this {} input, so it checked nothing",
                        if tool.is_empty() { "tool" } else { &tool }
                    ));
                }
            } else {
                unread = Some(format!(
                    "the blaze hook could not read which files this {} call wrote (no `tool_input.file_path` or patch), so it checked nothing",
                    if tool.is_empty() { "tool" } else { &tool }
                ));
            }
        }
    }
    let mut once = Vec::with_capacity(files.len()); // alloc: as above
    for file in files {
        if !once.contains(&file) {
            once.push(file);
        }
    }
    Ok(Input { event, session, cwd, tool, files: once, unread, command, scratch: text("scratchpad_dir") })
}

/// The cargo or rustc `command` (a shell command line) runs, as the words that say it
/// (`cargo build`, `rustc`): any of its pipeline's or list's commands, past environment
/// assignments and the wrappers that run what follows them. `cargo blaze ...` is blaze's.
pub fn cargo_in(command: &str) -> Option<String> {
    let simple = command.split(['\n', ';', '|', '&', '(', ')', '`']);
    for part in simple {
        let mut words = part.split_whitespace().map(|it| it.trim_matches(['"', '\'']));
        let mut first = None;
        for word in words.by_ref() {
            let wrapper = matches!(word, "env" | "time" | "command" | "exec" | "nice" | "nohup" | "sudo" | "xargs");
            let assignment = word.contains('=') && !word.starts_with('-') && !word.starts_with('=');
            if wrapper || assignment || (first.is_none() && word.starts_with('-')) {
                continue;
            }
            first = Some(word);
            break;
        }
        let Some(first) = first else { continue };
        let program = first.rsplit('/').next().unwrap_or(first);
        if program == "rustc" || program == "rustdoc" {
            return Some(program.to_owned());
        }
        if program != "cargo" {
            continue;
        }
        let sub = words.find(|it| !it.starts_with('+') && !it.starts_with('-'));
        match sub {
            Some(sub @ ("build" | "b" | "check" | "c" | "test" | "t" | "run" | "r" | "clippy" | "bench" | "doc" | "d")) => {
                return Some(format!("cargo {sub}")); // alloc: an answer, once
            }
            _ => continue,
        }
    }
    None
}

/// The before-command hook's answer to `found` (what [`cargo_in`] found): a word to the model
/// that blaze does it, or under `strict` a refusal of the command.
pub fn before_answer(found: &str, strict: bool) -> Value {
    let instead = match found {
        "rustc" | "rustdoc" => "`blaze build` (or `blaze doc`)".to_owned(),
        cargo => format!("`blaze {}` with the same flags", cargo.trim_start_matches("cargo ")),
    };
    let why = format!(
        "This project is built with blaze, which is cargo and rustc both: `{found}` would build it \
         again apart from blaze's daemon, which already has every edit. Run {instead} instead."
    );
    match strict {
        true => json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": why,
        } }),
        false => json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": why,
        } }),
    }
}

/// The package an edit of a file is of: what is checked for it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Owner {
    /// Its Cargo.toml, or the virtual workspace's.
    pub manifest: PathBuf,
    /// `None`: a virtual manifest (`--workspace`), or one that does not parse.
    pub package: Option<String>,
    /// Why the manifest does not read, when it does not.
    pub unparsed: Option<String>,
}

impl Owner {
    /// The `blaze check` words of its check.
    pub fn check(&self) -> Vec<String> {
        // alloc: a command line, once a check
        let mut words = vec!["check".to_owned(), "--manifest-path".to_owned(), self.manifest.display().to_string()];
        match (&self.package, &self.unparsed) {
            (Some(package), _) => words.extend(["-p".to_owned(), package.clone()]),
            (None, None) => words.push("--workspace".to_owned()),
            (None, Some(_)) => {}
        }
        words.push("--all-targets".to_owned());
        words
    }

    /// The command that shows what the hook left out.
    pub fn rerun(&self) -> String {
        match (&self.package, &self.unparsed) {
            (Some(package), _) => format!("blaze diagnostics -p {package}"),
            (None, None) => format!("blaze diagnostics --manifest-path {} --workspace", self.manifest.display()),
            (None, Some(_)) => format!("blaze diagnostics --manifest-path {}", self.manifest.display()),
        }
    }

    pub fn name(&self) -> String {
        match &self.package {
            Some(package) => package.clone(),
            None => self.manifest.display().to_string(),
        }
    }

    fn scope(&self) -> String {
        match &self.package {
            Some(package) => format!("-p {package}"),
            None => format!("--manifest-path {}", self.manifest.display()),
        }
    }
}

/// Whether an edit of `file` is checked: a Rust file or a manifest.
pub fn checked(file: &Path) -> bool {
    file.extension().is_some_and(|it| it == "rs") || file.file_name().is_some_and(|it| it == "Cargo.toml")
}

/// The owner of an edited file, from the Cargo.toml files at and above it, nearest first, each
/// with its text or why it could not be read: the nearest with `[package]`, or with
/// `[workspace]` (a virtual one); one that does not parse is checked by its path (blaze says
/// where it is wrong), the reason kept. `None` with no manifest.
pub fn owner_of(manifests: &[(PathBuf, Result<String, String>)]) -> Option<Owner> {
    let (nearest, _) = manifests.first()?;
    for (manifest, text) in manifests {
        let read = text.as_ref().map_err(String::clone).and_then(|text| {
            toml::from_str::<toml::Table>(text).map_err(|error| format!("{} does not read as TOML: {error}", manifest.display()))
        });
        let table = match read {
            Ok(table) => table,
            Err(why) => return Some(Owner { manifest: manifest.clone(), package: None, unparsed: Some(why) }),
        };
        if let Some(toml::Value::Table(package)) = table.get("package") {
            let name = package.get("name").and_then(toml::Value::as_str).map(str::to_owned);
            return Some(Owner { manifest: manifest.clone(), package: name, unparsed: None });
        }
        if table.contains_key("workspace") {
            return Some(Owner { manifest: manifest.clone(), package: None, unparsed: None });
        }
    }
    Some(Owner { manifest: nearest.clone(), package: None, unparsed: None })
}

/// What one package's check said, for the hook.
#[derive(Clone, PartialEq, Debug)]
pub enum Checked {
    /// The daemon's answer, with the edited files of the package (relative to the checkout)
    /// and the directory the agent runs in relative to it (`""` at its root, `None` outside),
    /// so names are shown as the agent would type them.
    Seen { owner: Owner, seen: Seen, edited: Vec<String>, cwd: Option<String> },
    /// The request itself failed: blaze, not the code.
    Failed { owner: Owner, what: String },
    /// The deadline passed before this package's check was asked for.
    Late { owner: Owner },
}

/// One package's result, as the hook says it.
struct Said {
    errors: String,
    context: Vec<String>,
    person: Option<String>,
}

fn plural(count: usize, word: &str) -> String {
    match count {
        1 => format!("1 {word}"),
        count => format!("{count} {word}s"),
    }
}

fn shown(file: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some("") | None => file.to_owned(),
        Some(cwd) => file
            .strip_prefix(cwd)
            .and_then(|it| it.strip_prefix('/'))
            .unwrap_or(file)
            .to_owned(),
    }
}

/// At most [`CAP`] of `found` drawn, within [`TEXT_BUDGET`] characters; and how many more.
fn capped(found: &[&Found], cwd: Option<&str>) -> (Vec<String>, usize) {
    let mut drawn = Vec::new(); // alloc: a hook's answer, once
    let mut used = 0;
    for one in found.iter().take(CAP) {
        let mut text = String::new();
        let mut shifted = (*one).clone(); // clone: drawn under the name the agent types
        if let Some(span) = shifted.span.as_mut() {
            span.file = shown(&span.file, cwd);
        }
        diagnostics::draw(&shifted, &mut text);
        if used + text.len() + 1 > TEXT_BUDGET {
            break;
        }
        used += text.len() + 1;
        drawn.push(text);
    }
    let more = found.len() - drawn.len();
    (drawn, more)
}

fn section(scope: &str, rerun: &str, head: String, drawn: &[String], more: usize) -> String {
    if drawn.is_empty() && more == 0 {
        return String::new();
    }
    let mut text = format!("blaze check {scope}: {head}:");
    for one in drawn {
        text.push('\n');
        text.push_str(one);
    }
    if more > 0 {
        text.push_str(&format!("\nand {more} more: run `{rerun}`"));
    }
    text
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect(); // alloc: a failure's text, once
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

fn said_of(checked: &Checked) -> Said {
    let (owner, seen, edited, cwd) = match checked {
        Checked::Failed { owner, what } => {
            let text = format!("blaze itself failed while checking {}, not the code: {}", owner.name(), tail(what));
            let first = text.lines().next().unwrap_or_default().to_owned();
            return Said { errors: String::new(), context: vec![text], person: Some(first) };
        }
        Checked::Late { owner } => {
            let text = format!(
                "blaze check did not answer in {} s: {} was not checked; run `{}` to see it",
                DEADLINE_MS / 1000,
                owner.name(),
                owner.rerun()
            );
            return Said { errors: String::new(), context: vec![text], person: None };
        }
        Checked::Seen { owner, seen, edited, cwd } => (owner, seen, edited, cwd.as_deref()),
    };
    let scope = owner.scope();
    let rerun = owner.rerun();
    if let Some(failed) = &seen.failed {
        let mut what = tail(&failed.what);
        if let Some(why) = &owner.unparsed {
            what = format!("{what}\n(and the manifest itself: {why})");
        }
        let text = format!("blaze itself failed while checking {}, not the code: {what}", owner.name());
        let first = text.lines().next().unwrap_or_default().to_owned();
        return Said { errors: String::new(), context: vec![text], person: Some(first) };
    }
    if !seen.ended {
        let text = format!(
            "blaze check did not answer in {} s: it is still running in the daemon; run `{rerun}` to see it",
            DEADLINE_MS / 1000
        );
        return Said { errors: String::new(), context: vec![text], person: None };
    }
    let mut context = Vec::new(); // alloc: a hook's answer, once
    let first = seen.base != Base::Held;
    let in_edit = |it: &&Found| it.span.as_ref().is_some_and(|span| edited.contains(&span.file));
    let new: Vec<&Found> = seen.new.iter().filter(|it| diagnostics::said(it)).collect();
    let (shown_new, elsewhere): (Vec<&Found>, Vec<&Found>) = match first {
        true => new.iter().copied().partition(in_edit),
        false => (new.clone(), Vec::new()),
    };
    let errors: Vec<&Found> = shown_new.iter().copied().filter(|it| it.level == Level::Error).collect();
    let warnings: Vec<&Found> = shown_new.iter().copied().filter(|it| it.level == Level::Warning).collect();
    let (drawn_errors, more_errors) = capped(&errors, cwd);
    let (drawn_warnings, more_warnings) = capped(&warnings, cwd);
    let file = edited.first().map(|it| it.rsplit('/').next().unwrap_or(it)).unwrap_or_default();
    let head = |count: usize, level: &str| match first {
        true => format!("{} in {file}", plural(count, level)),
        false => format!("{} in {}", plural(count, &format!("new {level}")), owner.name()),
    };
    let error_text = section(&scope, &rerun, head(errors.len(), "error"), &drawn_errors, more_errors);
    let warning_text = section(&scope, &rerun, head(warnings.len(), "warning"), &drawn_warnings, more_warnings);
    if !warning_text.is_empty() {
        context.push(warning_text);
    }
    let summary = match first {
        true => {
            let errors = elsewhere.iter().filter(|it| it.level == Level::Error).count();
            let warnings = elsewhere.iter().filter(|it| it.level == Level::Warning).count();
            match errors + warnings {
                0 => format!("blaze check {scope}: first check this session; nothing elsewhere in {}.", owner.name()),
                _ => format!(
                    "blaze check {scope}: first check this session; elsewhere in {}: {}, {} (`{rerun}` shows them).",
                    owner.name(),
                    plural(errors, "error"),
                    plural(warnings, "warning"),
                ),
            }
        }
        false => {
            let fixed = seen.fixed.iter().filter(|it| diagnostics::said(it)).count();
            let unchanged: Vec<&Found> = seen.unchanged.iter().filter(|it| diagnostics::said(it)).collect();
            let still = unchanged.iter().filter(|it| it.level == Level::Error).count();
            let new = match shown_new.len() {
                0 => "nothing new".to_owned(),
                count => format!("{count} new"),
            };
            let mut line = format!("blaze check {scope}: {new}, {fixed} fixed, {} unchanged.", unchanged.len());
            if still > 0 {
                line.push_str(&format!(" {} said before still there (`{rerun}` shows all).", plural(still, "error")));
            }
            line
        }
    };
    context.push(summary);
    if seen.base == Base::Unknown {
        context.push(format!(
            "the daemon no longer holds this session's last check of {} (it restarted, or let it go): shown as a first check",
            owner.name()
        ));
    }
    Said { errors: error_text, context, person: None }
}

/// The after-edit hook's answer to the checks of an edit, in the shape Claude Code and Codex
/// both read: new errors as a blocking `decision` whose `reason` the model sees in place of
/// the tool's result, so it fixes them first; warnings, counts and notes as
/// `hookSpecificOutput.additionalContext`; blaze's own failure also as a `systemMessage` the
/// person sees. `None`: nothing to say.
pub fn after_answer(agent: Agent, results: &[Checked], unread: &[String]) -> Option<Value> {
    let mut errors = Vec::new(); // alloc: a hook's answer, once
    let mut context = Vec::new(); // alloc: as above
    let mut person = Vec::new(); // alloc: as above
    for result in results {
        let said = said_of(result);
        if !said.errors.is_empty() {
            errors.push(said.errors);
        }
        context.extend(said.context);
        person.extend(said.person);
    }
    context.extend(unread.iter().cloned());
    if errors.is_empty() && context.is_empty() {
        return None;
    }
    let mut answer = Map::new();
    if !errors.is_empty() {
        answer.insert("decision".to_owned(), json!("block"));
        let reason = format!(
            "{} was applied, and blaze check found new errors in it; fix them before going on.\n{}",
            agent.edit_word(),
            errors.join("\n")
        );
        answer.insert("reason".to_owned(), json!(reason));
    }
    if !context.is_empty() {
        answer.insert(
            "hookSpecificOutput".to_owned(),
            json!({ "hookEventName": "PostToolUse", "additionalContext": context.join("\n") }),
        );
    }
    if !person.is_empty() {
        answer.insert("systemMessage".to_owned(), json!(person.join("\n")));
    }
    Some(Value::Object(answer))
}

/// The hook's own failure, said to both the model and the person: never a quiet pass.
pub fn hook_failed(event: &str, what: &str) -> Value {
    let text = format!("the blaze hook itself failed, so this was not checked: {what}");
    json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text }, "systemMessage": text })
}

#[cfg(test)]
mod tests;
