//! The transcript's tool calls as a reader takes them in, derived from the reduced items (never stored): a run of calls
//! with no words of the agent's between them is one [`Run`], summed up in plain words ("Read 2 files, ran 1 shell
//! command") the way Claude Code's terminal does; and which calls open by default ([`Grouped::default_open`]): only the
//! transcript's last item when it is a shell call that went well (a failure is one folded line). Fold state a person set lives in the app, by call id;
//! this is only the default it starts from. Agent-neutral: Claude's, Codex's and the harness REPL's calls alike.
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
    /// A skill's instructions loaded (the REPL's skill(name)).
    Skill,
    /// A harness REPL cell that stepped nowhere outside the REPL.
    Cell,
    Other,
}

impl Act {
    /// `n` of this act in words, lower case: "ran 3 shell commands", "searched once".
    #[must_use]
    pub fn words(self, n: usize) -> String {
        let s = |one: &str, many: &str| if n == 1 { one.to_owned() } else { many.to_owned() };
        match self {
            Act::Shell => format!("ran {n} shell {}", s("command", "commands")),
            Act::Read => format!("read {n} {}", s("file", "files")),
            Act::Edit => format!("edited {n} {}", s("file", "files")),
            Act::Write => format!("wrote {n} {}", s("file", "files")),
            Act::Delete => format!("deleted {n} {}", s("file", "files")),
            Act::Search if n == 1 => "searched once".to_owned(),
            Act::Search => format!("searched {n} times"),
            Act::List => format!("listed {n} {}", s("folder", "folders")),
            Act::Build => format!("ran {n} {}", s("build", "builds")),
            Act::Fetch => format!("fetched {n} {}", s("page", "pages")),
            Act::Query => format!("ran {n} {}", s("query", "queries")),
            Act::Spawn => format!("started {n} {}", s("session", "sessions")),
            Act::Skill => format!("read {n} {}", s("skill", "skills")),
            Act::Cell => format!("ran {n} {}", s("cell", "cells")),
            Act::Other => format!("used {n} {}", s("tool", "tools")),
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
            "skill" => Act::Skill,
            _ => Act::Other,
        }
    }

    /// A plain tool call's act by its name: Claude Code's tools, Codex's items (harness/src/chat/codex.rs).
    fn of_tool(name: &str) -> Act {
        match name.to_ascii_lowercase().as_str() {
            "bash" | "shell" | "local_shell" | "exec_command" | "bashoutput" => Act::Shell,
            "read" | "notebookread" | "view" => Act::Read,
            "edit" | "multiedit" | "notebookedit" | "apply_patch" | "str_replace" => Act::Edit,
            "write" => Act::Write,
            "grep" | "glob" | "ls" | "websearch" | "web_search" | "toolsearch" => Act::Search,
            "webfetch" | "fetch" => Act::Fetch,
            _ => Act::Other,
        }
    }
}

/// Acts in words, counted by kind in the order each kind first came, capitalised: "Read 3 files, searched 2 times".
#[must_use]
pub fn summary(acts: impl IntoIterator<Item = Act>) -> String {
    let mut counts: Vec<(Act, usize)> = Vec::new();
    for a in acts {
        match counts.iter_mut().find(|(k, _)| *k == a) {
            Some((_, n)) => *n += 1,
            None => counts.push((a, 1)),
        }
    }
    let s = counts.iter().map(|(a, n)| a.words(*n)).collect::<Vec<_>>().join(", ");
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// One tool call as the grouping sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The call's block id: the key its fold state is kept by.
    pub block: String,
    /// What it did, one act per step (a call that edited three files is three edits).
    pub acts: Vec<Act>,
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
        self.acts.contains(&Act::Shell)
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
    let acts = match detail {
        Some(d) if d.effects.is_empty() => vec![Act::Cell],
        Some(d) => d.effects.iter().map(|e| Act::of_effect(&e.kind)).collect(),
        // A Codex patch names every file it changed.
        None if name == "apply_patch" => {
            let n = input["paths"].as_array().map_or(1, |p| p.len().max(1));
            vec![Act::Edit; n]
        }
        None => vec![Act::of_tool(name)],
    };
    let failed = result.as_ref().is_some_and(|r| r.error)
        || detail.is_some_and(|d| d.status == "error" || d.effects.iter().any(|e| e.status == "error"));
    let running = match detail {
        Some(d) => d.status == "running",
        None => result.is_none() && (!done || t.busy),
    };
    Some(Call { block: block.clone(), acts, failed, running, draws: detail.is_some_and(|d| draws(&d.ui)) })
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

    /// What its calls did, in words, in the order each kind first came: "Read 2 files, ran 1 shell command".
    #[must_use]
    pub fn summary(&self) -> String {
        summary(self.calls.iter().flat_map(|(_, c)| c.acts.iter().copied()))
    }

    /// How many of its calls failed.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.calls.iter().filter(|(_, c)| c.failed).count()
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

/// The transcript's items as pieces, and which item is last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grouped {
    pub pieces: Vec<Piece>,
    /// The transcript's last item that shows (a turn's end and empty blocks show nothing): a shell call there stays open
    /// while the turn runs and once it ended, until something comes after it.
    pub last: Option<usize>,
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

fn shows(it: &Item) -> bool {
    match it {
        Item::TurnEnd { .. } => false,
        Item::Text { text, .. } | Item::Thinking { text, .. } => !text.trim().is_empty(),
        _ => true,
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
        let last = t.items.iter().rposition(shows);
        Grouped { pieces, last }
    }

    /// Whether call `c` (item `i`) is open before anyone folds or opens it: when it is a shell call that went well and the
    /// transcript's last item. Everything else starts folded, a failure too (one line in the failed tone that opens on a
    /// press); a run starts folded, its failed and open calls showing under it.
    #[must_use]
    pub fn default_open(&self, i: usize, c: &Call) -> bool {
        !c.failed && c.shell() && self.last == Some(i)
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

    /// Calls with only reasoning between them are one run, summed up in words by kind in the order each came; prose ends
    /// a run; a lone call is no run; a failed call stays folded and its run counts it; the last shell call opens.
    #[test]
    fn runs_group_and_default_open() {
        let mut lines = vec![serde_json::json!({"ev": "user", "text": "go"})];
        lines.extend(tool("a", "Read", serde_json::json!({"file_path": "x"}), "x", false));
        lines.push(serde_json::json!({"ev": "block", "block": "th", "kind": "thinking", "text": "hmm"}));
        lines.extend(tool("b", "Bash", serde_json::json!({"command": "false"}), "", true));
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
        assert_eq!(rs[0].summary(), "Read 2 files, ran 1 shell command");
        assert_eq!(rs[0].items, [1, 2, 3, 4], "the reasoning between its calls is inside it");
        assert_eq!(rs[0].failed(), 1);
        assert_eq!(rs[0].key(), "run:a");
        assert_eq!(rs[1].summary(), "Edited 2 files, ran 1 shell command");
        // The lone Grep between two prose blocks is an item of its own.
        assert!(g.pieces.contains(&Piece::Item(6)));
        let open: Vec<&str> = rs
            .iter()
            .flat_map(|r| &r.calls)
            .filter(|(i, c)| g.default_open(*i, c))
            .map(|(_, c)| c.block.as_str())
            .collect();
        assert_eq!(open, ["f"], "the last shell call; the failed one stays folded");

        // Something after the last shell call: it folds.
        let mut more = lines.clone();
        more.push(serde_json::json!({"ev": "user", "text": "thanks"}));
        let t = transcript(&more);
        let g = Grouped::of(&t);
        let (i, f) = runs(&g)[1].calls[1].clone();
        assert!(!g.default_open(i, &f));
    }

    /// A harness REPL cell counts its steps; one that draws a view stands alone, between runs, and never folds into one.
    #[test]
    fn cells_count_steps_and_views_stand_alone() {
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
        lines.extend(cell("d", serde_json::json!([{"kind": "search", "done": "Searched", "status": "ok"}]), serde_json::Value::Null));
        let t = transcript(&lines);
        let g = Grouped::of(&t);
        let rs = runs(&g);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].summary(), "Ran 3 shell commands");
        assert_eq!(rs[0].failed(), 1);
        assert!(g.pieces.contains(&Piece::Item(3)), "the view stands alone");
        assert_eq!(rs[1].summary(), "Ran 1 cell, searched once");
    }

    /// The REPL's typed calls count as what they did: reads, searches, listings and builds, never shell commands.
    #[test]
    fn typed_calls_sum_up_in_words() {
        let kinds = ["skill", "read", "read", "search", "read", "list", "search", "build"];
        assert_eq!(summary(kinds.map(Act::of_effect)), "Read 1 skill, read 3 files, searched 2 times, listed 1 folder, ran 1 build");
    }
}
