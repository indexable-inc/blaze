//! The transcript's tool calls as a reader takes them in, derived from the reduced items (never stored): a run of calls
//! with no words of the agent's between them is one [`Run`], summed up in plain words ("Read 2 files, ran 1 shell
//! command") the way Claude Code's terminal does. The words come from each step's own outcome: a step that went well
//! counts as what it did, one that failed as what it could not do ("Searched once, couldn't fetch 1 page"), so a cell
//! whose search worked and whose fetch failed never reads as a failed search; each failure carries one plain sentence
//! ([`Call::why`]). Every call starts folded; fold state a person set lives in the app, by call id. Agent-neutral:
//! Claude's, Codex's and the harness REPL's calls alike.
use crate::{Detail, Item, Transcript};
use serde_json::Value;

/// What one step of a call did, for counting in a run's summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Shell,
    Read,
    Edit,
    Write,
    Delete,
    Search,
    /// A directory listing or file walk (the REPL's ls, glob, du).
    List,
    /// A build, lint or test followed to a digest (the REPL's follow, cargo.*).
    Build,
    Fetch,
    Query,
    Spawn,
    /// Waiting for an earlier cell (the REPL's wait): bookkeeping, said only when it is all a run did.
    Wait,
    /// A skill's instructions loaded (the REPL's skill(name)).
    Skill,
    /// A harness REPL cell that stepped nowhere outside the REPL.
    Cell,
    Other,
}

impl Act {
    /// Its verb (past and plain) and what it counts (one, many).
    fn parts(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Act::Shell => ("ran", "run", "shell command", "shell commands"),
            Act::Read => ("read", "read", "file", "files"),
            Act::Edit => ("edited", "edit", "file", "files"),
            Act::Write => ("wrote", "write", "file", "files"),
            Act::Delete => ("deleted", "delete", "file", "files"),
            Act::Search => ("searched", "search", "", ""),
            Act::List => ("listed", "list", "folder", "folders"),
            Act::Build => ("ran", "run", "build", "builds"),
            Act::Fetch => ("fetched", "fetch", "page", "pages"),
            Act::Query => ("ran", "run", "query", "queries"),
            Act::Spawn => ("started", "start", "session", "sessions"),
            Act::Wait => ("waited for", "wait for", "cell", "cells"),
            Act::Skill => ("read", "read", "skill", "skills"),
            Act::Cell => ("ran", "run", "cell", "cells"),
            Act::Other => ("used", "use", "tool", "tools"),
        }
    }

    /// `n` of this act in words, lower case: "ran 3 shell commands", "searched once".
    #[must_use]
    pub fn words(self, n: usize) -> String {
        let (past, _, one, many) = self.parts();
        match self {
            Act::Search if n == 1 => "searched once".to_owned(),
            Act::Search => format!("searched {n} times"),
            _ => format!("{past} {n} {}", if n == 1 { one } else { many }),
        }
    }

    /// `n` of this act that failed, in words, lower case: "couldn't fetch 1 page", "couldn't search".
    #[must_use]
    pub fn failed_words(self, n: usize) -> String {
        let (_, base, one, many) = self.parts();
        match self {
            Act::Search if n == 1 => "couldn't search".to_owned(),
            Act::Search => format!("couldn't search {n} times"),
            _ => format!("couldn't {base} {n} {}", if n == 1 { one } else { many }),
        }
    }

    /// A harness REPL step's kind (mcp/src/effects.ts).
    #[must_use]
    pub fn of_effect(kind: &str) -> Act {
        match kind {
            "exec" => Act::Shell,
            "read" => Act::Read,
            "write" => Act::Write,
            "delete" => Act::Delete,
            "net" => Act::Fetch,
            "search" => Act::Search,
            "list" => Act::List,
            "build" => Act::Build,
            "sql" => Act::Query,
            "spawn" => Act::Spawn,
            "wait" => Act::Wait,
            "skill" => Act::Skill,
            _ => Act::Other,
        }
    }

    /// A plain tool call's act by its name: Claude Code's tools, Codex's items (harness/src/chat/codex.rs).
    #[must_use]
    pub fn of_tool(name: &str) -> Act {
        match name.to_ascii_lowercase().as_str() {
            "bash" | "shell" | "local_shell" | "exec_command" | "bashoutput" => Act::Shell,
            "read" | "notebookread" | "view" => Act::Read,
            "edit" | "multiedit" | "notebookedit" | "apply_patch" | "str_replace" => Act::Edit,
            "write" => Act::Write,
            "grep" | "glob" | "ls" | "websearch" | "web_search" | "toolsearch" => Act::Search,
            "webfetch" | "fetch" => Act::Fetch,
            "task" | "agent" => Act::Spawn,
            _ => Act::Other,
        }
    }
}

impl Act {
    /// Where it comes in a run's words, what matters first: what changed (edits, writes, deletes), then what ran
    /// (builds, commands, sessions, queries), then what was looked at; waiting last.
    fn rank(self) -> u8 {
        match self {
            Act::Edit => 0,
            Act::Write => 1,
            Act::Delete => 2,
            Act::Build => 3,
            Act::Shell => 4,
            Act::Spawn => 5,
            Act::Query => 6,
            Act::Fetch => 7,
            Act::Search => 8,
            Act::Cell => 9,
            Act::Other => 10,
            Act::Read => 11,
            Act::List => 12,
            Act::Skill => 13,
            Act::Wait => 14,
        }
    }

    /// Only looking (a file read, a folder listed, a skill loaded, a wait): not worth saying beside anything else.
    fn looks(self) -> bool {
        matches!(self, Act::Read | Act::List | Act::Skill | Act::Wait)
    }
}

/// Acts counted by kind, what matters first ([`Act::rank`]): `(act, n)`.
fn counted(acts: impl IntoIterator<Item = Act>) -> Vec<(Act, usize)> {
    let mut counts: Vec<(Act, usize)> = Vec::new();
    for a in acts {
        match counts.iter_mut().find(|(k, _)| *k == a) {
            Some((_, n)) => *n += 1,
            None => counts.push((a, 1)),
        }
    }
    counts.sort_by_key(|(a, _)| a.rank());
    counts
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// One kind of act in words, by name where every one of them has one: "edited chat.rs and tokens.flecs", "ran cargo
/// test", past three "edited a.rs, b.rs and 4 more files"; else counted, "edited 2 files".
fn named_words(act: Act, n: usize, names: &[(Act, String)]) -> String {
    let mut mine: Vec<&str> = Vec::new();
    for (a, name) in names {
        if *a == act && !mine.contains(&name.as_str()) {
            mine.push(name);
        }
    }
    let (past, _, one, many) = act.parts();
    let named = mine.len().min(n);
    if named == 0 || (named < n && named < NAMED) {
        return act.words(n);
    }
    let shown = &mine[..named.min(NAMED)];
    let rest = n - shown.len();
    let list = match (shown, rest) {
        ([a], 0) => (*a).to_owned(),
        (_, 0) => format!("{} and {}", shown[..shown.len() - 1].join(", "), shown[shown.len() - 1]),
        _ => format!("{} and {rest} more {}", shown.join(", "), if rest == 1 { one } else { many }),
    };
    format!("{past} {list}")
}

/// The most names one kind of act says before counting the rest.
const NAMED: usize = 3;

/// Acts in words, counted by kind, what matters first, capitalised: "Edited 2 files, ran 1 build".
#[must_use]
pub fn summary(acts: impl IntoIterator<Item = Act>) -> String {
    told(acts, [])
}

/// What went well and what failed, in words: the acts that went well counted by kind, what changed first, then those
/// that failed ("Edited 2 files, couldn't run 1 build"), capitalised. Looking (reads, listings, skills, waits) is said
/// only when nothing that went well did more than look: the person cares what changed, not what was read on the way.
#[must_use]
pub fn told(ok: impl IntoIterator<Item = Act>, failed: impl IntoIterator<Item = Act>) -> String {
    told_named(ok, &[], failed)
}

/// [`told`], each kind that went well said by what it was done to where its names are known ([`named_words`]):
/// "Edited chat.rs and tokens.flecs, ran cargo test".
#[must_use]
pub fn told_named(ok: impl IntoIterator<Item = Act>, names: &[(Act, String)], failed: impl IntoIterator<Item = Act>) -> String {
    let (ok, failed) = (counted(ok), counted(failed));
    let did = ok.iter().any(|(a, _)| !a.looks());
    let busy = ok.iter().chain(&failed).any(|(a, _)| *a != Act::Wait);
    let keep_ok = |(a, _): &&(Act, usize)| if did { !a.looks() } else { !busy || *a != Act::Wait };
    let keep_failed = |(a, _): &&(Act, usize)| !busy || *a != Act::Wait;
    let words: Vec<String> = ok
        .iter()
        .filter(keep_ok)
        .map(|(a, n)| named_words(*a, *n, names))
        .chain(failed.iter().filter(keep_failed).map(|(a, n)| a.failed_words(*n)))
        .collect();
    capitalised(&words.join(", "))
}

/// One plain sentence for a failure (for the person: the model reads the whole error elsewhere): its first line, its
/// first sentence, no trailing stop, at most [`WHY_CHARS`] characters; a command's "exit 2" as words.
#[must_use]
pub fn plain(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if let Some(code) = line.strip_prefix("exit ").filter(|c| c.parse::<i64>().is_ok()) {
        return format!("Exited with code {code}");
    }
    let one = line.find(". ").map_or(line, |i| &line[..i]).trim_end_matches('.');
    match one.char_indices().nth(WHY_CHARS) {
        Some((i, _)) => format!("{}\u{2026}", &one[..i]),
        None => one.to_owned(),
    }
}

/// The most characters a failure's sentence keeps.
pub const WHY_CHARS: usize = 140;

/// One tool call as the grouping sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The call's block id: the key its fold state is kept by.
    pub block: String,
    /// What it did, one act per step that went well or still runs (a call that edited three files is three edits).
    pub acts: Vec<Act>,
    /// What it could not do: one act per step that failed (a cell that threw with no step of its own: one Cell).
    pub fails: Vec<Act>,
    /// What each act that went well was done to, where it names one ([`name_of`]: a file, a command, a host), in order:
    /// what a run's words say instead of a bare count.
    pub names: Vec<(Act, String)>,
    /// Each failure in one plain sentence ([`plain`]), in order.
    pub why: Vec<String>,
    /// It failed: an error result, a cell that threw, a step that failed (a command's non-zero exit).
    pub failed: bool,
    /// Still running.
    pub running: bool,
    /// It draws a view (a harness REPL cell's generated UI): the view is the point of the call, so it never folds away
    /// and never joins a run.
    pub draws: bool,
}

impl Call {
    /// A shell call: a command (Claude's Bash, Codex's shell) or a cell that ran one.
    #[must_use]
    pub fn shell(&self) -> bool {
        self.acts.contains(&Act::Shell) || self.fails.contains(&Act::Shell)
    }

    /// What it did, in words ([`told`]).
    #[must_use]
    pub fn summary(&self) -> String {
        told_named(self.acts.iter().copied(), &self.names, self.fails.iter().copied())
    }
}

/// Whether a shown value or genui tree has nothing to draw (null, an empty list of views).
fn draws(ui: &Value) -> bool {
    match ui {
        Value::Null => false,
        Value::Array(a) => a.iter().any(draws),
        Value::Object(o) => o.contains_key("type"),
        _ => false,
    }
}

/// Item `i` as a call, if it is one.
#[must_use]
pub fn call(t: &Transcript, i: usize) -> Option<Call> {
    let Item::Tool { block, name, input, result, done, .. } = t.items.get(i)? else { return None };
    let detail: Option<&Detail> = t.details.get(block);
    let (mut acts, mut fails, mut why, mut names) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    match detail {
        Some(d) => {
            for e in &d.effects {
                let a = Act::of_effect(&e.kind);
                if e.status == "error" {
                    fails.push(a);
                    why.push(plain(&e.detail));
                } else {
                    acts.push(a);
                    names.extend(name_of(a, &e.target).map(|n| (a, n)));
                }
            }
            if d.status == "error" {
                // A cell that threw: its own error, said once (a step's failure it rethrew is said already).
                let msg = plain(d.value["message"].as_str().unwrap_or(""));
                if !msg.is_empty() && !why.contains(&msg) {
                    why.push(msg);
                }
                if fails.is_empty() {
                    fails.push(Act::Cell);
                }
            } else if d.effects.is_empty() {
                acts.push(Act::Cell);
            }
        }
        None => {
            // A Codex patch names every file it changed.
            let n = if name == "apply_patch" { input["paths"].as_array().map_or(1, |p| p.len().max(1)) } else { 1 };
            let a = Act::of_tool(name);
            match result {
                Some(r) if r.error => {
                    fails.extend(std::iter::repeat_n(a, n));
                    why.push(plain(&r.text));
                }
                _ => {
                    acts.extend(std::iter::repeat_n(a, n));
                    names.extend(tool_targets(name, input).into_iter().filter_map(|t| name_of(a, &t)).map(|n| (a, n)));
                }
            }
        }
    }
    why.retain(|w| !w.is_empty());
    let failed = !fails.is_empty();
    let running = match detail {
        Some(d) => d.status == "running",
        None => result.is_none() && (!done || t.busy),
    };
    Some(Call { block: block.clone(), acts, fails, names, why, failed, running, draws: detail.is_some_and(|d| draws(&d.ui)) })
}

/// What a plain tool call (Claude Code's, Codex's) was done to, from its input: a file's path, a patch's paths, a
/// command, a page, a sub-agent's description.
fn tool_targets(name: &str, input: &Value) -> Vec<String> {
    let s = |k: &str| input[k].as_str().map(str::to_owned);
    if name == "apply_patch" {
        return input["paths"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    }
    ["file_path", "notebook_path", "path", "command", "cmd", "url", "description"].iter().find_map(|k| s(k)).into_iter().collect()
}

/// The name a run's words give what `act` was done to (`target`: a path, a command, a URL): a file by its name, a
/// command by its first words, a page by its host, a session by what it was asked; None for what has no useful name
/// (a search, a wait, a cell).
fn name_of(act: Act, target: &str) -> Option<String> {
    let t = target.lines().map(str::trim).find(|l| !l.is_empty())?;
    let cut = |s: &str, n: usize| match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s.to_owned(),
    };
    match act {
        Act::Read | Act::Edit | Act::Write | Act::Delete | Act::List => {
            Some(t.trim_end_matches('/').rsplit('/').next().unwrap_or(t).to_owned()).filter(|n| !n.is_empty())
        }
        Act::Shell | Act::Build => {
            // Past `cd <dir> &&` and leading VAR=value: the program and its first words.
            let mut rest = t;
            while let Some(after) = rest.strip_prefix("cd ") {
                match after.find("&&") {
                    Some(i) => rest = after[i + 2..].trim_start(),
                    None => break,
                }
            }
            let words: Vec<&str> = rest
                .split_whitespace()
                .skip_while(|w| w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())))
                .take(3)
                .map(|w| if w.starts_with('/') { w.rsplit('/').next().unwrap_or(w) } else { w })
                .collect();
            (!words.is_empty()).then(|| cut(&words.join(" "), 32))
        }
        Act::Fetch => t.split("://").nth(1).and_then(|r| r.split('/').next()).map(str::to_owned),
        Act::Spawn => Some(cut(t, 32)),
        _ => None,
    }
}

/// Calls in a row, with what came between them that is not the agent's words (its reasoning, a text block not yet
/// written to): a run's members in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Item indices from the run's first call to its last, the reasoning between them included.
    pub items: Vec<usize>,
    /// Its calls (item index and call), in order.
    pub calls: Vec<(usize, Call)>,
}

impl Run {
    /// The key a run's own fold state is kept by: its first call's id (a run only grows at its end).
    #[must_use]
    pub fn key(&self) -> String {
        format!("run:{}", self.calls.first().map_or("", |(_, c)| c.block.as_str()))
    }

    /// What its calls did, in words, in the order each kind first came: "Read 2 files, ran 1 shell command"; what failed
    /// after, as what could not be done ([`told`]).
    #[must_use]
    pub fn summary(&self) -> String {
        let names: Vec<(Act, String)> = self.calls.iter().flat_map(|(_, c)| c.names.iter().cloned()).collect();
        told_named(self.calls.iter().flat_map(|(_, c)| c.acts.iter().copied()), &names, self.calls.iter().flat_map(|(_, c)| c.fails.iter().copied()))
    }

    /// How many of its calls failed.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.calls.iter().filter(|(_, c)| c.failed).count()
    }

    /// Its failures, one plain sentence each, in order, each said once.
    #[must_use]
    pub fn why(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in self.calls.iter().flat_map(|(_, c)| &c.why) {
            if !out.contains(w) {
                out.push(w.clone());
            }
        }
        out
    }

    /// One of its calls is still running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.calls.iter().any(|(_, c)| c.running)
    }
}

/// A stretch of the transcript as drawn: one item, or a run of calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Item(usize),
    Run(Run),
}

/// The transcript's items as pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grouped {
    pub pieces: Vec<Piece>,
}

/// An item that draws nothing (a block not yet written to, reasoning with no words) or only reasoning: it neither ends a
/// run nor counts in one.
fn quiet(it: &Item) -> bool {
    match it {
        Item::Text { text, .. } => text.trim().is_empty(),
        Item::Thinking { .. } => true,
        _ => false,
    }
}

impl Grouped {
    /// Group a transcript's items.
    #[must_use]
    pub fn of(t: &Transcript) -> Self {
        let mut pieces = Vec::new();
        // The calls (and quiet items after each) gathered since the last thing that ends a run.
        let mut pending: Vec<usize> = Vec::new();
        let mut calls: Vec<(usize, Call)> = Vec::new();
        let flush = |pending: &mut Vec<usize>, calls: &mut Vec<(usize, Call)>, pieces: &mut Vec<Piece>| {
            if calls.len() >= 2 {
                // Quiet items after the last call stay outside the run: nothing came after them yet.
                let last = calls.last().map_or(0, |(i, _)| *i);
                let first = calls[0].0;
                let (inside, after): (Vec<usize>, Vec<usize>) = pending.iter().partition(|&&i| i <= last);
                let (before, inside): (Vec<usize>, Vec<usize>) = inside.into_iter().partition(|&i| i < first);
                pieces.extend(before.into_iter().map(Piece::Item));
                pieces.push(Piece::Run(Run { items: inside, calls: std::mem::take(calls) }));
                pieces.extend(after.into_iter().map(Piece::Item));
            } else {
                pieces.extend(pending.iter().map(|&i| Piece::Item(i)));
            }
            pending.clear();
            calls.clear();
        };
        for (i, it) in t.items.iter().enumerate() {
            match call(t, i) {
                Some(c) if !c.draws => {
                    pending.push(i);
                    calls.push((i, c));
                }
                Some(_) => {
                    flush(&mut pending, &mut calls, &mut pieces);
                    pieces.push(Piece::Item(i));
                }
                None if quiet(it) => pending.push(i),
                None => {
                    flush(&mut pending, &mut calls, &mut pieces);
                    pieces.push(Piece::Item(i));
                }
            }
        }
        flush(&mut pending, &mut calls, &mut pieces);
        Grouped { pieces }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::event::Event;

    fn transcript(lines: &[serde_json::Value]) -> Transcript {
        let mut t = Transcript::default();
        for l in lines {
            t.apply(&Event::from_json(l).unwrap());
        }
        t
    }

    fn tool(id: &str, name: &str, input: serde_json::Value, out: &str, error: bool) -> [serde_json::Value; 2] {
        [
            serde_json::json!({"ev": "tool_call", "block": id, "name": name, "input": input}),
            serde_json::json!({"ev": "tool_result", "block": id, "text": out, "error": error}),
        ]
    }

    fn runs(g: &Grouped) -> Vec<&Run> {
        g.pieces.iter().filter_map(|p| if let Piece::Run(r) = p { Some(r) } else { None }).collect()
    }

    /// Calls with only reasoning between them are one run, summed up in words by kind in the order each came, what
    /// failed said as what could not be done; prose ends a run; a lone call is no run.
    #[test]
    fn runs_group_and_say_what_failed() {
        let mut lines = vec![serde_json::json!({"ev": "user", "text": "go"})];
        lines.extend(tool("a", "Read", serde_json::json!({"file_path": "x"}), "x", false));
        lines.push(serde_json::json!({"ev": "block", "block": "th", "kind": "thinking", "text": "hmm"}));
        lines.extend(tool("b", "Bash", serde_json::json!({"command": "false"}), "boom. more", true));
        lines.extend(tool("c", "Read", serde_json::json!({"file_path": "y"}), "y", false));
        lines.push(serde_json::json!({"ev": "block", "block": "m", "kind": "text", "text": "Now searching."}));
        lines.extend(tool("d", "Grep", serde_json::json!({"pattern": "p"}), "", false));
        lines.push(serde_json::json!({"ev": "block", "block": "m2", "kind": "text", "text": "And:"}));
        lines.extend(tool("e", "apply_patch", serde_json::json!({"paths": ["a", "b"]}), "completed", false));
        lines.extend(tool("f", "shell", serde_json::json!({"command": "ls"}), "a b", false));
        lines.push(serde_json::json!({"ev": "turn_end", "ok": true, "stop": "end_turn"}));
        let t = transcript(&lines);
        let g = Grouped::of(&t);
        let rs = runs(&g);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].summary(), "Read x and y, couldn't run 1 shell command", "what it was done to, by name");
        assert_eq!(rs[0].why(), ["boom"]);
        assert_eq!(rs[0].items, [1, 2, 3, 4], "the reasoning between its calls is inside it");
        assert_eq!(rs[0].failed(), 1);
        assert_eq!(rs[0].key(), "run:a");
        assert_eq!(rs[1].summary(), "Edited a and b, ran ls");
        // The lone Grep between two prose blocks is an item of its own.
        assert!(g.pieces.contains(&Piece::Item(6)));
    }

    /// A harness REPL cell counts its steps by how each went; one that draws a view stands alone, between runs, and
    /// never folds into one; a wait is said only when it is all that was done.
    #[test]
    fn cells_count_steps_by_outcome_and_views_stand_alone() {
        let cell = |id: &str, effects: serde_json::Value, ui: serde_json::Value| {
            let mut v = tool(id, "ts", serde_json::json!({"code": "x"}), "", false).to_vec();
            v.push(serde_json::json!({"ev": "tool_detail", "block": id, "status": "done", "ms": 3, "code": "x",
                "effects": effects, "value": null, "ui": ui}));
            v
        };
        let exec = |code: u8| {
            serde_json::json!({"kind": "exec", "done": "Ran x", "status": if code == 0 { "ok" } else { "error" },
                "detail": format!("exit {code}")})
        };
        let mut lines = vec![serde_json::json!({"ev": "user", "text": "go"})];
        lines.extend(cell("a", serde_json::json!([exec(0), exec(0)]), serde_json::Value::Null));
        lines.extend(cell("b", serde_json::json!([exec(1)]), serde_json::Value::Null));
        lines.extend(cell("v", serde_json::json!([]), serde_json::json!({"type": "Text", "props": {}, "children": []})));
        lines.extend(cell("c", serde_json::json!([]), serde_json::Value::Null));
        lines.extend(cell(
            "d",
            serde_json::json!([
                {"kind": "search", "done": "Searched", "status": "ok"},
                {"kind": "net", "done": "Fetched localhost", "status": "error", "detail": "No browser is connected"},
                {"kind": "wait", "done": "Waited", "status": "ok"}
            ]),
            serde_json::Value::Null,
        ));
        let t = transcript(&lines);
        let g = Grouped::of(&t);
        let rs = runs(&g);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].summary(), "Ran 2 shell commands, couldn't run 1 shell command");
        assert_eq!(rs[0].why(), ["Exited with code 1"]);
        assert!(g.pieces.contains(&Piece::Item(3)), "the view stands alone");
        assert_eq!(rs[1].summary(), "Searched once, ran 1 cell, couldn't fetch 1 page");
        assert_eq!(rs[1].why(), ["No browser is connected"]);
        assert_eq!(told([Act::Wait], []), "Waited for 1 cell");
    }

    /// The REPL's typed calls count as what they did: reads, searches, listings and builds, never shell commands; what
    /// ran leads, and the looking on the way is left unsaid beside it.
    #[test]
    fn typed_calls_sum_up_in_words() {
        let kinds = ["skill", "read", "read", "search", "read", "list", "search", "build"];
        assert_eq!(summary(kinds.map(Act::of_effect)), "Ran 1 build, searched 2 times");
        let looked = ["read", "list", "read", "skill"];
        assert_eq!(summary(looked.map(Act::of_effect)), "Read 2 files, listed 1 folder, read 1 skill", "only looking is said when it is all");
        let edits: Vec<(Act, String)> = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"].map(|n| (Act::Edit, n.to_owned())).into();
        assert_eq!(named_words(Act::Edit, 5, &edits), "edited a.rs, b.rs, c.rs and 2 more files", "past three, the rest counted");
        assert_eq!(named_words(Act::Shell, 1, &[(Act::Shell, "cargo test".into())]), "ran cargo test");
    }
}
