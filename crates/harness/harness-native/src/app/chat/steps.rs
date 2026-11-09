//! A turn's steps (scene/widgets/steps.flecs): outputs, minimal. Every call starts folded, and nothing opens unless the
//! person presses it. A run of calls, or a lone cell, is one quiet line ([`Line`], `hn.StepLine`) of marks, not words: a
//! glyph a kind of step and how many (a terminal and 2, a globe and 2), a small mark when something failed (its plain
//! sentence on hover). What it did in plain words (harness-chat's `group::told`: "Searched once, couldn't fetch 1
//! page") and how long it took are its tooltip. Opened, each
//! step shows its meaningful output only, one prefab per kind of output ([`Out`], `hn.Step*`): search results as titled
//! links, a fetched page as its host, a file as its chip, a command's output, a failure as one sentence, any other value
//! as one line of it that opens to its table. Never bookkeeping: no task ids, statuses, timestamps, empty logs or code.
//! Which glyph each kind draws is data too (`hn.StepKind.<kind>`).
use super::*;
use harness_native_ui_text_api::Icon;

/// Search results a query shows before saying how many more.
const RESULTS: usize = 5;
/// Lines of a command's output shown.
const OUTPUT_LINES: usize = 8;
/// Characters of a value's one-line preview.
const PREVIEW_CHARS: usize = 160;

/// The one line a run of calls or a lone cell folds to.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Line {
    /// What a press toggles (a run's `run:` key, a cell's block).
    pub key: String,
    /// What it did as marks, not words: each kind of step (`hn.StepKind.<kind>`) and how many, in the order they first
    /// happened ([`tally`]).
    pub tally: Vec<(&'static str, usize)>,
    /// While it runs, the kind of step it is on now (`think` between steps): the face its block shows (`ui.Block`),
    /// turning as the step changes, in place of the marks. None once it is done.
    pub live: Option<&'static str>,
    /// The same in words ("Ran cargo test, fetched 2 pages"): its tooltip, and what a copy takes.
    pub words: String,
    /// Its failures, a plain sentence a line (the mark's tooltip); empty when nothing failed.
    pub why: String,
    /// How long it took (its tooltip).
    pub tip: String,
    pub open: bool,
}

impl Line {
    pub(super) fn prefab(&self) -> &'static str {
        match (self.live.is_some(), self.why.is_empty()) {
            (true, _) => "hn.StepLineLive",
            (false, true) => "hn.StepLine",
            (false, false) => "hn.StepLineFailed",
        }
    }
}

/// One row of an opened line: a step's output, by its kind.
#[derive(Clone, Debug)]
pub(super) enum Out {
    /// A step's head: its kind's icon and what it did; failed, a mark and its plain sentence after.
    Head { kind: &'static str, words: String, why: String, tip: String },
    /// A fetched page: the verb, its host as a link, its path muted.
    Page { verb: String, url: String, tip: String },
    /// A file read, written or deleted: the verb, then its chip.
    File { verb: String, path: String, tip: String },
    /// Search results; a query's own under its words when they came grouped (`more`: how many not shown).
    Results { query: String, items: Rc<Vec<view::table::Result>>, more: usize },
    /// Words a cell gave.
    Text(String),
    /// Output a terminal or a program printed: the code face.
    Code(String),
    /// A value of no known kind: one line of it, opening to its table (`key`: what opens it).
    Value { key: String, preview: String, open: bool },
    /// A failure with no step of its own (a cell that threw): one plain sentence.
    Fail(String),
    /// Something said about the steps ("3 more steps, not kept").
    Note(String),
}

impl Out {
    pub(super) fn prefab(&self) -> &'static str {
        match self {
            Out::Head { why, .. } if why.is_empty() => "hn.StepHead",
            Out::Head { .. } => "hn.StepHeadFailed",
            Out::Page { .. } => "hn.StepPage",
            Out::File { .. } => "hn.StepFile",
            Out::Results { query, .. } if query.is_empty() => "hn.StepResults",
            Out::Results { .. } => "hn.StepResultsQuery",
            Out::Text(_) => "hn.StepText",
            Out::Code(_) => "hn.StepCode",
            Out::Value { .. } => "hn.StepValue",
            Out::Fail(_) => "hn.StepFail",
            Out::Note(_) => "hn.StepNote",
        }
    }

    /// Its words a selection can cover (see chat.rs `texts`).
    pub(super) fn texts(&self) -> Vec<(&'static [&'static str], String, bool)> {
        match self {
            Out::Head { words, .. } => vec![(&["line", "words"], words.clone(), false)],
            Out::Text(s) => vec![(&["text"], s.clone(), false)],
            Out::Code(s) => vec![(&["card", "text"], s.clone(), false)],
            Out::Fail(s) => vec![(&["line", "why"], s.clone(), false)],
            Out::Note(s) => vec![(&["text"], s.clone(), false)],
            Out::Page { .. } | Out::File { .. } | Out::Results { .. } | Out::Value { .. } => Vec::new(),
        }
    }

    /// It as markdown, for a copy that takes it whole.
    pub(super) fn markdown(&self) -> Option<String> {
        Some(match self {
            Out::Head { words, why, .. } if why.is_empty() => words.clone(),
            Out::Head { words, why, .. } => format!("{words}: {why}"),
            Out::Page { url, .. } => url.clone(),
            Out::File { verb, path, .. } => format!("{verb} `{path}`"),
            Out::Results { query, items, .. } => {
                let line = |r: &view::table::Result| match (r.title.is_empty(), r.url.is_empty()) {
                    (_, true) => r.title.clone(),
                    (true, false) => r.url.clone(),
                    (false, false) => format!("[{}]({})", r.title, r.url),
                };
                let list = items.iter().map(|r| format!("- {}", line(r))).collect::<Vec<_>>().join("\n");
                if query.is_empty() { list } else { format!("{query}\n{list}") }
            }
            Out::Text(s) | Out::Fail(s) | Out::Note(s) => s.clone(),
            Out::Code(s) => format!("```\n{s}\n```"),
            Out::Value { preview, .. } => preview.clone(),
        })
    }
}

/// An act's kind, as `hn.StepKind` names its glyph.
fn kind_of(a: Act) -> &'static str {
    match a {
        Act::Shell => "exec",
        Act::Read => "read",
        Act::Edit => "edit",
        Act::Write => "write",
        Act::Delete => "delete",
        Act::Search => "search",
        Act::List => "list",
        Act::Build => "build",
        Act::Fetch => "net",
        Act::Query => "sql",
        Act::Spawn => "spawn",
        Act::Wait => "wait",
        Act::Skill => "skill",
        Act::Cell => "cell",
        Act::Other => "tool",
    }
}

/// What a set of calls did, as a line's marks: each kind of step and how many, in the order they first happened.
/// Waiting counts only when nothing else happened; calls that did nothing are one `cell`.
fn tally<'c>(calls: impl IntoIterator<Item = &'c group::Call>) -> Vec<(&'static str, usize)> {
    let acts: Vec<Act> = calls.into_iter().flat_map(|c| c.acts.iter().chain(&c.fails).copied()).collect();
    let only_waits = acts.iter().all(|a| *a == Act::Wait);
    let mut out: Vec<(&'static str, usize)> = Vec::new(); // alloc: a few kinds a line
    for kind in acts.iter().filter(|a| only_waits || **a != Act::Wait).map(|a| kind_of(*a)) {
        match out.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => out.push((kind, 1)),
        }
    }
    if out.is_empty() {
        out.push(("cell", 1));
    }
    out
}

/// The kind of step `calls` are on now: the newest step of the newest call still running; `think` when none runs (the
/// agent is between steps).
fn now_kind<'c>(calls: impl DoubleEndedIterator<Item = &'c group::Call>) -> &'static str {
    calls.rev().find(|c| c.running).and_then(|c| c.acts.last().copied()).map_or("think", kind_of)
}

/// "Took 1.2s", or nothing while it runs or when it was not timed.
fn took(ms: Option<u64>) -> String {
    ms.map(|ms| format!("Took {}", secs(ms))).unwrap_or_default()
}

/// What cell `d` is doing now, when a step of it runs: that step's words.
fn doing(d: &Detail) -> Option<String> {
    if d.status != "running" {
        return None;
    }
    Some(d.effects.iter().rev().find(|e| e.status == "running").map_or_else(|| "Running code\u{2026}".to_owned(), |e| format!("{}\u{2026}", e.doing)))
}

/// A turn's one line, all its steps folded under it (only its last answer shows beside it): what one of its `calls` is
/// doing now while it runs (`live`), else what they did, named ("Edited chat.rs, ran cargo test"), failures marked;
/// how long the turn took (`ms`) its tooltip.
pub(super) fn turn_line(key: &str, calls: &[group::Call], t: &Transcript, ms: Option<u64>, live: bool, open: bool) -> Line {
    let now = if live { calls.iter().rev().filter(|c| c.running).find_map(|c| t.details.get(&c.block).and_then(doing)) } else { None };
    let names: Vec<(Act, String)> = calls.iter().flat_map(|c| c.names.iter().cloned()).collect();
    let did = group::told_named(calls.iter().flat_map(|c| c.acts.iter().copied()), &names, calls.iter().flat_map(|c| c.fails.iter().copied()));
    let words = match now {
        Some(n) => n,
        None if live && did.is_empty() => "Working\u{2026}".to_owned(),
        None if live => format!("{did}\u{2026}"),
        None if did.is_empty() => "Thought it through".to_owned(),
        None => did,
    };
    let mut why: Vec<String> = Vec::new();
    for w in calls.iter().flat_map(|c| &c.why) {
        if !why.contains(w) {
            why.push(w.clone());
        }
    }
    let tip = ms.map(|ms| format!("Worked {}", secs(ms))).unwrap_or_default();
    Line { key: key.to_owned(), tally: tally(calls), live: live.then(|| now_kind(calls.iter())), words, why: why.join("\n"), tip, open }
}

/// A lone cell's line: what it is doing now, else what it did ([`group::Call::summary`]).
pub(super) fn call_line(key: &str, c: &group::Call, d: &Detail, open: bool) -> Line {
    let words = doing(d).unwrap_or_else(|| c.summary());
    let tip = if d.status == "running" { String::new() } else { took(d.ms) };
    Line { key: key.to_owned(), tally: tally([c]), live: c.running.then(|| now_kind([c].into_iter())), words, why: c.why.join("\n"), tip, open }
}

/// A run's line: what its running call is doing now, else what its calls did ([`group::Run::summary`]), how long they
/// took together.
pub(super) fn run_line(key: &str, run: &group::Run, t: &Transcript, open: bool) -> Line {
    let details = || run.calls.iter().filter_map(|(_, c)| t.details.get(&c.block));
    let now = run.running().then(|| details().filter_map(doing).last()).flatten();
    let words = now.unwrap_or_else(|| run.summary());
    let ms: Vec<u64> = details().filter_map(|d| d.ms).collect();
    let tip = if run.running() || ms.is_empty() { String::new() } else { took(Some(ms.iter().sum())) };
    Line { key: key.to_owned(), tally: tally(run.calls.iter().map(|(_, c)| c)), live: run.running().then(|| now_kind(run.calls.iter().map(|(_, c)| c))), words, why: run.why().join("\n"), tip, open }
}

/// What a failed step could not do, from its words while it ran ("Searching Google for x" → "Couldn't search Google
/// for x"); its done words when its verb is not one the kernel says.
pub(super) fn tried(doing: &str, done: &str) -> String {
    const VERBS: [(&str, &str); 30] = [
        ("Searching", "search"),
        ("Fetching", "fetch"),
        ("Reading", "read"),
        ("Writing", "write"),
        ("Running", "run"),
        ("Listing", "list"),
        ("Measuring", "measure"),
        ("Querying", "query"),
        ("Starting", "start"),
        ("Saving", "save"),
        ("Moving", "move"),
        ("Deleting", "delete"),
        ("Creating", "create"),
        ("Copying", "copy"),
        ("Appending", "append"),
        ("Converting", "convert"),
        ("Sending", "send"),
        ("Replying", "reply"),
        ("Forwarding", "forward"),
        ("Archiving", "archive"),
        ("Trashing", "trash"),
        ("Relabelling", "relabel"),
        ("Marking", "mark"),
        ("Discarding", "discard"),
        ("Waiting", "wait"),
        ("Checking", "check"),
        ("Building", "build"),
        ("Testing", "test"),
        ("Linting", "lint"),
        ("Following", "follow"),
    ];
    let (first, rest) = doing.split_once(' ').unwrap_or((doing, ""));
    match VERBS.iter().find(|(ing, _)| *ing == first) {
        Some((_, base)) if rest.is_empty() => format!("Couldn't {base}"),
        Some((_, base)) => format!("Couldn't {base} {rest}"),
        None => done.to_owned(),
    }
}

/// Whether a step's target names a file (a path, not "labels" or a Gmail thread).
fn is_path(s: &str) -> bool {
    !s.is_empty() && !s.contains(char::is_whitespace) && (s.contains('/') || s.contains('.'))
}

/// Cell `block`'s outputs, opened: each step's ([`effect_outs`]), then the cell's own value, its failure and what it
/// printed. Its code never shows here (a drawn view's code button shows it).
pub(super) fn outs(block: &str, d: &Detail, fold: &HashMap<String, bool>, out: &mut Vec<Row>) {
    for (k, e) in d.effects.iter().enumerate() {
        effect_outs(&format!("{block}#v{k}"), e, fold, out);
    }
    if d.dropped > 0 {
        out.push(Row::Out(Out::Note(format!("{} more steps, not kept", d.dropped))));
    }
    if d.status == "error" {
        // A failure a step said already is not said again.
        let why = group::plain(d.value["message"].as_str().unwrap_or(""));
        let said = d.effects.iter().any(|e| e.status == "error" && group::plain(&e.detail) == why);
        if !why.is_empty() && !said {
            out.push(Row::Out(Out::Fail(why)));
        }
    } else {
        value_outs(&format!("{block}#v"), &d.value, fold, out);
    }
    if !d.logs.trim().is_empty() {
        out.push(Row::Out(Out::Code(head_lines(&d.logs, OUTPUT_LINES))));
    }
}

/// One step's rows: its head (a page's host, a file's chip, or its words), then its output.
fn effect_outs(key: &str, e: &model::chat::Effect, fold: &HashMap<String, bool>, out: &mut Vec<Row>) {
    let kind = kind_of(Act::of_effect(&e.kind));
    let mut tip = [step_detail(&e.detail), e.ms.map(secs).unwrap_or_default()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    if e.status == "running" {
        out.push(Row::Out(Out::Head { kind, words: format!("{}\u{2026}", e.doing), why: String::new(), tip: String::new() }));
        return;
    }
    if e.status == "error" {
        let why = group::plain(&e.detail);
        tip.retain(|t| *t != e.detail);
        out.push(Row::Out(Out::Head { kind, words: tried(&e.doing, &e.done), why, tip: tip.join(" \u{00B7} ") }));
        return;
    }
    let tip = tip.join(" \u{00B7} ");
    let verb = || e.done.split_whitespace().next().unwrap_or("").to_owned();
    match e.kind.as_str() {
        // Waiting is bookkeeping: what shows is the value the awaited cell gave.
        "wait" => {}
        // A fetched page is its host and path; its body is the agent's to read, not the person's.
        "net" if e.target.starts_with("http://") || e.target.starts_with("https://") => {
            out.push(Row::Out(Out::Page { verb: verb(), url: e.target.clone(), tip }));
            return;
        }
        "read" | "write" | "delete" if is_path(&e.target) && empty_value(&e.value) => {
            out.push(Row::Out(Out::File { verb: verb(), path: e.target.clone(), tip }));
        }
        _ => out.push(Row::Out(Out::Head { kind, words: e.done.clone(), why: String::new(), tip })),
    }
    // A command's output is the terminal's: the code face.
    if e.kind == "exec" && e.value["t"] == "text" {
        let s = e.value["v"].as_str().unwrap_or("");
        if !s.trim().is_empty() {
            out.push(Row::Out(Out::Code(head_lines(s, OUTPUT_LINES))));
        }
        return;
    }
    value_outs(key, &e.value, fold, out);
}

/// A step's detail as its tooltip says it: a command that exited 0 says nothing more.
fn step_detail(d: &str) -> String {
    if d == "exit 0" { String::new() } else { d.to_owned() }
}

/// The rows a shown value (mcp/src/shown.ts) is drawn as, by its kind: search results as links, words as words, an
/// error as its sentence, anything else as one line of it that opens (by `key`) to its table.
fn value_outs(key: &str, v: &Arc<Value>, fold: &HashMap<String, bool>, out: &mut Vec<Row>) {
    if empty_value(v) {
        return;
    }
    if v["kind"] == "search" {
        out.extend(results(v).into_iter().map(|(query, items, more)| Row::Out(Out::Results { query, items: Rc::new(items), more })));
        return;
    }
    match v["t"].as_str().unwrap_or("") {
        "text" => {
            let s = v["v"].as_str().unwrap_or("").trim_end();
            // Lines of it are a file's or a program's: the code face; a sentence reads as words.
            out.push(Row::Out(if s.lines().count() > 1 { Out::Code(head_lines(s, OUTPUT_LINES)) } else { Out::Text(s.to_owned()) }));
        }
        "scalar" => out.push(Row::Out(Out::Text(cell_text(&v["v"])))),
        "error" => out.push(Row::Out(Out::Fail(group::plain(v["message"].as_str().unwrap_or(""))))),
        "table" | "record" | "list" => {
            let open = fold.get(key).copied().unwrap_or(false);
            out.push(Row::Out(Out::Value { key: key.to_owned(), preview: preview(v), open }));
            if open {
                out.push(Row::Grid { v: v.clone(), cap: usize::MAX });
            }
        }
        // A page the cell drove in the harness browser: its live card (genui's Browser, src/app/browser.rs).
        "browser" => out.push(Row::Ui { v: Arc::new(genui::shown_card(v)), cell: String::new(), block: String::new(), code: false }),
        _ => {}
    }
}

/// One line of a table, record or list: a list's items, a record's fields, a table's size and first column.
fn preview(v: &Value) -> String {
    let rows: &[Value] = v["rows"].as_array().map_or(&[], Vec::as_slice);
    let total = v["total"].as_u64().map_or(rows.len(), |t| t as usize);
    let one = |x: &Value| {
        let s = cell_text(x).split_whitespace().collect::<Vec<_>>().join(" ");
        if s.is_empty() { "\u{2014}".to_owned() } else { s }
    };
    let more = |shown: usize| if total > shown { format!(" +{} more", total - shown) } else { String::new() };
    let s = match v["t"].as_str().unwrap_or("") {
        "list" => {
            let items: Vec<String> = rows.iter().take(6).map(|r| one(&r[0])).collect();
            format!("{}{}", items.join(", "), more(items.len()))
        }
        "record" => {
            let fields: Vec<String> = rows.iter().take(4).map(|r| format!("{}: {}", cell_text(&r[0]), one(&r[1]))).collect();
            format!("{}{}", fields.join(" \u{00B7} "), more(fields.len()))
        }
        _ => {
            let firsts: Vec<String> = rows.iter().take(3).map(|r| one(&r[0])).collect();
            format!("{total} {}: {}", if total == 1 { "row" } else { "rows" }, firsts.join(", "))
        }
    };
    match s.char_indices().nth(PREVIEW_CHARS) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s,
    }
}

/// Search results (title, url, site, an optional query) grouped by their query in the order each first came: each
/// group's first [`RESULTS`], and how many more it had.
fn results(v: &Value) -> Vec<(String, Vec<view::table::Result>, usize)> {
    let cols: Vec<&str> = v["cols"].as_array().into_iter().flatten().map(|c| c["name"].as_str().unwrap_or("")).collect();
    let at = |name: &str| cols.iter().position(|c| *c == name);
    let (title, url, site, query) = (at("title"), at("url"), at("site"), at("query"));
    let rows: &[Value] = v["rows"].as_array().map_or(&[], Vec::as_slice);
    let get = |r: &Value, c: Option<usize>| c.map(|c| cell_text(&r[c])).filter(|s| s != "null").unwrap_or_default();
    let mut groups: Vec<(String, Vec<view::table::Result>, usize)> = Vec::new();
    for r in rows {
        let q = get(r, query);
        let i = match groups.iter().position(|(g, ..)| *g == q) {
            Some(i) => i,
            None => {
                groups.push((q, Vec::new(), 0));
                groups.len() - 1
            }
        };
        let g = &mut groups[i];
        if g.1.len() >= RESULTS {
            g.2 += 1;
            continue;
        }
        let url = get(r, url);
        let host = if url.contains("google.") || url.is_empty() {
            get(r, site).split(['\u{203A}', ' ']).next().unwrap_or("").to_owned()
        } else {
            view::table::host(&url).to_owned()
        };
        g.1.push(view::table::Result { title: get(r, title), url, host, snippet: String::new() });
    }
    let total = v["total"].as_u64().map_or(rows.len(), |t| t as usize);
    if let Some(last) = groups.last_mut() {
        last.2 += total.saturating_sub(rows.len());
    }
    groups
}

/// A step's kind, as `hn.StepKind` names its glyph (the composer's activity cards list steps by it too).
pub(super) fn effect_kind(e: &model::chat::Effect) -> &'static str {
    kind_of(Act::of_effect(&e.kind))
}

/// The glyph `hn.StepKind.<kind>` draws.
pub(super) fn kind_glyph(world: WorldRef<'_>, kind: &str) -> String {
    glyph(world, kind)
}

/// The glyph `hn.StepKind.<kind>` draws.
fn glyph(world: WorldRef<'_>, kind: &str) -> String {
    view::prefab(world, &format!("hn.StepKind.{kind}"))
        .and_then(|e| e.try_cloned::<&Icon>())
        .map(|i| i.glyph.as_str().to_owned())
        .unwrap_or_else(|| panic!("the scene's hn.StepKind has no {kind}"))
}

/// Fill a line's look (`hn.StepLook`): its kind's icon and its words.
fn look(v: View<'_>, kind: &str, words: &str) {
    if let Some(i) = v.part("icon") {
        i.icon(&glyph(v.entity().world(), kind));
    }
    if let Some(w) = v.part("words") {
        w.text(words);
    }
}

/// Fill a pressable line's chevron, turned to `open`.
fn chevron(v: View<'_>, open: bool) {
    if let Some(c) = v.part("chevron") {
        c.icon(crate::look::CHEVRON);
        crate::look::disclose(c.entity(), open);
    }
}

/// Fill a run's or a cell's line.
pub(super) fn fill_line(row: View<'_>, l: &Line) {
    let Some(h) = row.part("head") else { return };
    h.action(&format!("chat:open:{}", l.key));
    // Marks, not words: a glyph a kind of step, its count after it when there was more than one. The words are the
    // tooltip (with how long it took).
    // While it runs: its block, showing the step it is on (it turns as the step changes), and no marks yet.
    if let (Some(b), Some(kind)) = (h.part("block"), l.live) {
        let e = b.entity();
        let mut block = e.try_cloned::<&harness_native_ui_text_api::PixelBlock>().unwrap_or_default();
        if block.face.as_str() != kind {
            block.face = kind.into();
            e.set(block);
        }
        // The step it is on, to the ear (src/audio.rs: a new one plays its cue).
        crate::audio::step(kind);
    }
    let tally: &[(&'static str, usize)] = if l.live.is_some() { &[] } else { &l.tally };
    if let Some(items) = h.part("items") {
        let world = h.entity().world();
        view::for_each(items, "hn.StepTally", tally.iter().map(|&(kind, n)| (kind, (kind, n))), |v, (kind, n), _| {
            if let Some(i) = v.part("icon") {
                i.icon(&glyph(world, kind));
            }
            if let Some(c) = v.part("n") {
                c.text(&if n > 1 { n.to_string() } else { String::new() });
            }
        });
    }
    chevron(h, l.open);
    tooltip(h, &[l.words.as_str(), l.tip.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" \u{b7} "));
    if let Some(m) = h.part("mark") {
        tooltip(m, &l.why);
    }
}

/// Fill one output row.
pub(super) fn fill(row: View<'_>, o: &Out) {
    match o {
        Out::Head { kind, words, why, tip } => {
            if let Some(l) = row.part("line") {
                look(l, kind, words);
                tooltip(l, tip);
                if let Some(w) = l.part("why") {
                    w.text(why);
                }
            }
        }
        Out::Page { verb, url, tip } => {
            if let Some(l) = row.part("line") {
                look(l, "net", verb);
                tooltip(l, tip);
                let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
                let path = rest.find('/').map_or("", |i| &rest[i..]);
                if let Some(k) = l.part("link") {
                    k.text(view::table::host(url));
                    k.action(&format!("open:{url}"));
                    tooltip(k, url);
                }
                if let Some(p) = l.part("path") {
                    p.text(if path == "/" { "" } else { path });
                }
            }
        }
        Out::File { verb, path, tip } => {
            if let Some(l) = row.part("line") {
                let kind = if verb == "Read" { "read" } else if verb == "Deleted" { "delete" } else { "write" };
                look(l, kind, verb);
                tooltip(l, tip);
                if let Some(c) = l.part("chip") {
                    if let Some(i) = c.part("icon") {
                        i.drawing(genui::file_icon(path));
                    }
                    if let Some(n) = c.part("name") {
                        n.text(path.trim_end_matches('/').rsplit('/').next().unwrap_or(path));
                    }
                    if let Some(n) = c.part("line") {
                        n.text("");
                    }
                    c.action(&format!("open:{path}"));
                    tooltip(c, path);
                }
            }
        }
        Out::Results { query, items, more } => {
            if let Some(q) = row.part("query") {
                q.text(query);
            }
            if let Some(list) = row.part("list") {
                view::table::results(list, items, "hn.StepResult");
            }
            if let Some(f) = row.part("foot") {
                foot(f, &if *more > 0 { format!("{more} more") } else { String::new() });
            }
        }
        Out::Text(s) | Out::Note(s) => {
            if let Some(t) = row.part("text") {
                t.text(s);
            }
        }
        Out::Code(s) => {
            if let Some(t) = row.part("card").and_then(|c| c.part("text")) {
                t.text(s);
            }
        }
        Out::Value { key, preview, open } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{key}"));
                if let Some(w) = h.part("words") {
                    w.text(preview);
                }
                chevron(h, *open);
            }
        }
        Out::Fail(s) => {
            if let Some(w) = row.part("line").and_then(|l| l.part("why")) {
                w.text(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed step says what it could not do; a value of no known kind is one line; grouped results keep their query.
    #[test]
    fn steps_say_outcomes_and_values_in_a_line() {
        assert_eq!(tried("Searching Google for \"x\"", "Searched Google for \"x\""), "Couldn't search Google for \"x\"");
        assert_eq!(tried("Frobbing", "Frobbed"), "Frobbed");
        let list = serde_json::json!({"t": "list", "total": 1, "rows": [["WebSocket"]], "cols": [{"name": "value", "type": "string"}]});
        assert_eq!(preview(&list), "WebSocket");
        let rec = serde_json::json!({"t": "record", "total": 2, "rows": [["web", "No browser is connected"], ["mail", ""]],
            "cols": [{"name": "key", "type": "string"}, {"name": "value", "type": "string"}]});
        assert_eq!(preview(&rec), "web: No browser is connected \u{00B7} mail: \u{2014}");
        let hits = serde_json::json!({"t": "table", "kind": "search", "total": 3,
            "cols": [{"name": "title"}, {"name": "url"}, {"name": "query"}],
            "rows": [["A", "https://a.com/x", "q1"], ["B", "https://b.com", "q1"], ["C", "https://c.com", "q2"]]});
        let g = results(&hits);
        assert_eq!(g.iter().map(|(q, i, _)| (q.as_str(), i.len())).collect::<Vec<_>>(), [("q1", 2), ("q2", 1)]);
        assert_eq!(g[0].1[0].host, "a.com");
    }
}
