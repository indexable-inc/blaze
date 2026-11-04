//! harness-native's world: what the daemons hold, as flecs entities, components and relations. The UI is a view over this.
//!
//! - A [`Device`] is a daemon (the tree's root; one today, several later), its link state the exclusive `(Link, Up |
//!   Connecting | Down)`.
//! - An [`Object`] is one of its sessions: `(Kind, Claude | Codex | ...)`, `(Status, Running | Exited | Lost)`, [`Name`],
//!   [`Title`], [`Cwd`], [`ExitCode`] once it ended, and flecs's own `ChildOf` to its parent object or its device.
//! - A shell run (`r12`) is an entity with [`Run`], `(Kind, Proc)`, [`Name`] (its command), `(Status, Running | Exited |
//!   Killed | Lost)`, [`ExitCode`], [`Created`] / [`Ended`], and `ChildOf` its session (or the device). A run split into
//!   pipeline stages has one child per stage: [`StageNo`], `(Kind, Stage)`, its text, its own status, exit code and times.
//! - A run whose session the daemon does not list goes under a group node (`(Kind, Group)`, [`GroupKey`], [`Name`]) on
//!   the device: one "REPL runs" per REPL (the run's `repl`) or per gone session, else one "other runs", all below the live sessions.
//! - [`Unfolded`] on a session (or the device) shows all its runs; without it the tree folds all but the last few.
//!   [`Collapsed`] on a node hides all its children (every run group starts so).
//! - [`Archived`] on an object takes it (and what is under it) out of the tree and into the sidebar's Archive.
//! - `(Viewing, object)` on the app entity is what the main pane shows (exclusive).
//! - An attached object has [`Term`]: its grid's size and how far it is scrolled back; the terminal (harness-vt) itself is the
//!   host's, in [`Terms`] by session id (a parser is no plain data a component can hold).
//!
//! The daemon link (a thread in the app) sends [`Msg`]s; the `DaemonInbox` system applies them to the world each frame.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use harness_native_api::APP;
use harness_native_flecs::prelude::*;

/// A daemon: where its socket is.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Device {
    pub host: ScriptString,
    pub sock: ScriptString,
}

/// `(Link, state)`: whether the app is connected to the device.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Link;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Up;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Connecting;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Down;

/// A daemon session, by its id there.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Object {
    pub id: u64,
}

/// `(Kind, kind)`: what runs in it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Kind;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Claude;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Codex;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CursorAgent;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Shell;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Proc;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Stage;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Group;

/// `(Status, status)`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Status;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Running;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Exited;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Lost;
/// A run ended by a signal.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Killed;
/// A stage that has not started yet.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pending;
/// A stage its run ended without starting (`&&` / `||`).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Skipped;

#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Name {
    pub s: ScriptString,
}

/// The title the program set (OSC 0/2).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Title {
    pub s: ScriptString,
}

#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Cwd {
    pub s: ScriptString,
}

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ExitCode {
    pub code: i32,
}

/// A daemon shell run (`r12`), by its number.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub n: u64,
}

/// A pipeline stage of the run it is a child of, by its index there (`r12.0`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageNo {
    pub k: u32,
}

/// When it ended (ms since the epoch).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Ended {
    pub ms: u64,
}

/// On a session or the device: the tree shows all its runs, not only the last [`KEEP_RUNS`].
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Unfolded;

/// On a node: the tree shows none of its children.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Collapsed;

/// On an object: the user killed it with cmd+W (or the palette's or menu's Kill). It leaves the tree for the sidebar's
/// Archive, where it stays selectable; an object that ended on its own stays in the tree.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Archived;

/// A group of runs with no listed session, by what they share (`repl:<id>`, `session:<n>`, `other`).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct GroupKey {
    pub s: ScriptString,
}

/// Finished runs a parent shows while folded (running ones always show).
pub const KEEP_RUNS: usize = 3;

/// When it was made (ms since the epoch): the tree's order among siblings.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Created {
    pub ms: u64,
}

/// `(Viewing, object)` on the app entity: the object the main pane shows.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Viewing;

/// An attached object's terminal: its grid in cells, how many rows it is scrolled back, and a count that moves whenever
/// its screen changed (what the view redraws on).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Term {
    pub cols: u16,
    pub rows: u16,
    pub scroll: u32,
    pub seq: u32,
}

/// The inbox singleton: frames of daemon messages applied so far.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Inbox {
    pub applied: u32,
}

/// The terminals of attached objects, by session id.
pub type Terms = Rc<RefCell<HashMap<u64, harness_vt::Term>>>;

/// Rows of scrollback a terminal keeps.
pub const SCROLLBACK: usize = 10_000;

/// What the daemon link sends the world.
#[derive(Debug)]
pub enum Msg {
    Link(LinkState),
    /// Every session the daemon has, as its `list` op returns them.
    List(Vec<serde_json::Value>),
    /// The attach stream's bytes (the snapshot first).
    Data { id: u64, bytes: Vec<u8> },
    Exit { id: u64, code: i32 },
    /// The attach stream ended.
    Detached { id: u64 },
    /// The daemon's recent shell runs, as its `runs` op returns them (newest first).
    Runs(Vec<serde_json::Value>),
    /// Output of run `r{run}` (of its stage `stage`, or the caller-visible streams), as `run_read` returned it.
    RunData { run: u64, stage: Option<u32>, err: bool, bytes: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Up,
    Connecting,
    Down,
}

pub fn register(world: &World) {
    world.component::<Device>().writable();
    world.component::<Link>().add_trait::<flecs::Exclusive>();
    for_tags(world);
    world.component::<Object>().writable();
    world.component::<Kind>().add_trait::<flecs::Exclusive>();
    world.component::<Status>().add_trait::<flecs::Exclusive>();
    world.component::<Name>().writable();
    world.component::<Title>().writable();
    world.component::<Cwd>().writable();
    world.component::<ExitCode>().writable();
    world.component::<Created>().writable();
    world.component::<Run>().writable();
    world.component::<StageNo>().writable();
    world.component::<Ended>().writable();
    world.component::<Unfolded>();
    world.component::<Collapsed>();
    world.component::<Archived>();
    world.component::<GroupKey>().writable();
    world.component::<Viewing>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Term>().writable();
    world.component::<Inbox>().writable();
    world.set(Inbox::default());
}

fn for_tags(world: &World) {
    world.component::<Up>();
    world.component::<Connecting>();
    world.component::<Down>();
    world.component::<Claude>();
    world.component::<Codex>();
    world.component::<CursorAgent>();
    world.component::<Shell>();
    world.component::<Proc>();
    world.component::<Stage>();
    world.component::<Group>();
    world.component::<Running>();
    world.component::<Exited>();
    world.component::<Lost>();
    world.component::<Killed>();
    world.component::<Pending>();
    world.component::<Skipped>();
}

/// The kind tag's id for the daemon's kind string.
fn kind_id(w: WorldRef<'_>, kind: &str) -> u64 {
    match kind {
        "claude" => *w.component_id::<Claude>(),
        "codex" => *w.component_id::<Codex>(),
        "cursor" | "cursor-agent" => *w.component_id::<CursorAgent>(),
        "shell" | "sh" | "zsh" | "bash" | "fish" | "nu" => *w.component_id::<Shell>(),
        "stage" => *w.component_id::<Stage>(),
        _ => *w.component_id::<Proc>(),
    }
}

/// A grid side as vt100 takes it (at least 1).
#[must_use]
pub fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(1)).unwrap_or(std::num::NonZeroU16::MIN)
}

/// The local device: the one daemon on this machine.
pub const LOCAL: &str = "devices::local";

/// Make the local device and install the inbox: each frame it drains `rx` into the world. Returns the terminal parsers:
/// the sessions' (by session id) and the runs' and stages' (by entity id).
pub fn install(world: WorldRef<'_>, host: &str, sock: &str, rx: Receiver<Msg>) -> (Terms, Terms) {
    let dev = world.entity_named(LOCAL);
    dev.set(Device { host: host.into(), sock: sock.into() });
    dev.add((Link::id(), Connecting::id()));
    let dev = *dev.id();
    let terms: Terms = Rc::default();
    let run_terms: Terms = Rc::default();
    let parsers = (terms.clone(), run_terms.clone());
    let mut idx = Index::default();
    world.system_named::<&mut Inbox>("DaemonInbox").each_iter(move |it, _, inbox| {
        let w = it.world();
        for msg in rx.try_iter() {
            inbox.applied = inbox.applied.wrapping_add(1);
            apply(w, dev, &mut idx, &parsers, msg);
        }
    });
    (terms, run_terms)
}

/// Daemon ids to entities: sessions by id, runs by number, stages by (run number, index).
#[derive(Default)]
struct Index {
    sessions: HashMap<u64, u64>,
    runs: HashMap<u64, u64>,
    stages: HashMap<(u64, u32), u64>,
    groups: HashMap<String, u64>,
}

fn apply(w: WorldRef<'_>, dev: u64, idx: &mut Index, (terms, run_terms): &(Terms, Terms), msg: Msg) {
    let by_id = &mut idx.sessions;
    match msg {
        Msg::Link(s) => {
            let tag = match s {
                LinkState::Up => *w.component_id::<Up>(),
                LinkState::Connecting => *w.component_id::<Connecting>(),
                LinkState::Down => *w.component_id::<Down>(),
            };
            w.entity_from_id(dev).add((Link::id(), tag));
        }
        Msg::List(sessions) => reconcile(w, dev, by_id, &sessions),
        Msg::Data { id, bytes } => {
            let Some(e) = by_id.get(&id).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) else { return };
            let mut t = e.try_cloned::<&Term>().unwrap_or(Term { cols: 80, rows: 24, scroll: 0, seq: 0 });
            let mut map = terms.borrow_mut();
            let p = map.entry(id).or_insert_with(|| harness_vt::Term::new(t.cols, t.rows, SCROLLBACK));
            p.feed(&bytes);
            t.seq = t.seq.wrapping_add(1);
            e.set(t);
        }
        Msg::Exit { id, code } => {
            if let Some(e) = by_id.get(&id).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) {
                e.set(ExitCode { code });
                e.add((Status::id(), Exited::id()));
            }
        }
        // The stream ended (the program exited, or the daemon went): the last screen stays to read; the host attaches
        // again (on a fresh parser) when it wants the live one.
        Msg::Detached { .. } => {}
        Msg::Runs(runs) => reconcile_runs(w, dev, idx, &runs),
        Msg::RunData { run, stage, err, bytes } => {
            let ent = match stage {
                Some(k) => idx.stages.get(&(run, k)),
                None => idx.runs.get(&run),
            };
            let Some(e) = ent.map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) else { return };
            let mut t = e.try_cloned::<&Term>().unwrap_or(Term { cols: 80, rows: 24, scroll: 0, seq: 0 });
            let mut map = run_terms.borrow_mut();
            let p = map.entry(*e.id()).or_insert_with(|| {
                // Read-only output: no cursor.
                let mut p = harness_vt::Term::new(t.cols, t.rows, SCROLLBACK);
                p.feed(b"\x1b[?25l");
                p
            });
            p.feed(&plain_output(&bytes, err));
            t.seq = t.seq.wrapping_add(1);
            e.set(t);
        }
    }
}

/// A run's output for a terminal parser: it is a program's plain output, not a terminal's, so a bare newline also
/// returns the carriage; stderr is tinted red.
fn plain_output(bytes: &[u8], err: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 32);
    if err {
        out.extend_from_slice(b"\x1b[38;2;235;107;107m");
    }
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    if err {
        out.extend_from_slice(b"\x1b[39m");
    }
    out
}

/// Set the exclusive `(rel, tag)` when it is not already the target.
fn set_pair(e: EntityView<'_>, rel: u64, tag: u64) {
    if e.target(rel, 0).is_none_or(|t| *t.id() != tag) {
        e.add((rel, tag));
    }
}

fn set_name(e: EntityView<'_>, name: &str) {
    if e.try_cloned::<&Name>().is_none_or(|n| n.s.as_str() != name) {
        e.set(Name { s: name.into() });
    }
}

fn set_times(e: EntityView<'_>, start: Option<u64>, end: Option<u64>) {
    let created = Created { ms: start.unwrap_or(0) };
    if e.try_cloned::<&Created>() != Some(created) {
        e.set(created);
    }
    match end {
        Some(ms) => {
            if e.try_cloned::<&Ended>() != Some(Ended { ms }) {
                e.set(Ended { ms });
            }
        }
        None => {
            if e.has(Ended::id()) {
                e.remove(Ended::id());
            }
        }
    }
}

fn set_code(e: EntityView<'_>, code: Option<i64>) {
    match code {
        Some(c) => {
            if e.try_cloned::<&ExitCode>() != Some(ExitCode { code: c as i32 }) {
                e.set(ExitCode { code: c as i32 });
            }
        }
        None => {
            if e.has(ExitCode::id()) {
                e.remove(ExitCode::id());
            }
        }
    }
}

/// Bring the runs in line with the daemon's `runs` answer: each a child of its session (or the device), its stages
/// children of it when it was split into more than one.
fn reconcile_runs(w: WorldRef<'_>, dev: u64, idx: &mut Index, runs: &[serde_json::Value]) {
    let (kind, status) = (*w.component_id::<Kind>(), *w.component_id::<Status>());
    let proc_ = *w.component_id::<Proc>();
    let stage_kind = *w.component_id::<Stage>();
    let tag = |s: &str| match s {
        "running" => *w.component_id::<Running>(),
        "killed" => *w.component_id::<Killed>(),
        "lost" | "unknown" => *w.component_id::<Lost>(),
        "pending" => *w.component_id::<Pending>(),
        "skipped" => *w.component_id::<Skipped>(),
        _ => *w.component_id::<Exited>(),
    };
    let group_kind = *w.component_id::<Group>();
    let mut seen = std::collections::HashSet::new();
    let mut seen_stages = std::collections::HashSet::new();
    // Group key -> its newest run's start.
    let mut groups_seen: HashMap<String, u64> = HashMap::new();
    for r in runs {
        let Some(n) = r["id"].as_str().and_then(|s| s.strip_prefix('r')).and_then(|s| s.parse::<u64>().ok()) else { continue };
        seen.insert(n);
        let e = match idx.runs.get(&n).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) {
            Some(e) => e,
            None => {
                let e = w.entity();
                e.set(Run { n });
                idx.runs.insert(n, *e.id());
                e
            }
        };
        set_pair(e, kind, proc_);
        set_pair(e, status, tag(r["status"].as_str().unwrap_or("")));
        set_code(e, r["exit_code"].as_i64());
        set_name(e, r["cmd"].as_str().unwrap_or("").trim());
        let cwd = r["cwd"].as_str().unwrap_or("");
        if e.try_cloned::<&Cwd>().is_none_or(|c| c.s.as_str() != cwd) {
            e.set(Cwd { s: cwd.into() });
        }
        set_times(e, r["start_ms"].as_u64(), r["end_ms"].as_u64());
        let listed = r["session"].as_u64().and_then(|s| idx.sessions.get(&s).copied()).filter(|&p| w.entity_from_id(p).is_alive());
        let parent = match listed {
            Some(p) => p,
            None => {
                // No listed session: one group per REPL or gone session, else "other runs" (collapsed at first).
                let (key, name) = match (r["repl"].as_str().filter(|s| !s.is_empty()), r["session"].as_u64()) {
                    (Some(repl), _) => (format!("repl:{repl}"), format!("REPL runs \u{00B7} {repl}")),
                    (None, Some(s)) => (format!("session:{s}"), format!("REPL runs \u{00B7} s{s}")),
                    (None, None) => ("other".to_owned(), "other runs".to_owned()),
                };
                let start = r["start_ms"].as_u64().unwrap_or(0);
                let newest = groups_seen.entry(key.clone()).or_insert(0);
                *newest = (*newest).max(start);
                match idx.groups.get(&key).map(|&g| w.entity_from_id(g)).filter(|g| g.is_alive()) {
                    Some(g) => *g.id(),
                    None => {
                        let g = w.entity();
                        g.set(GroupKey { s: key.as_str().into() });
                        g.add((Kind::id(), group_kind));
                        g.set(Name { s: name.as_str().into() });
                        // Runs whose session is gone are history: every group starts collapsed.
                        g.add(Collapsed::id());
                        g.child_of(dev);
                        idx.groups.insert(key, *g.id());
                        *g.id()
                    }
                }
            }
        };
        if e.parent().is_none_or(|p| *p.id() != parent) {
            e.child_of(parent);
        }
        let stages = r["stages"].as_array().map(Vec::as_slice).unwrap_or_default();
        if stages.len() < 2 {
            continue;
        }
        for (k, s) in stages.iter().enumerate() {
            let k = k as u32;
            seen_stages.insert((n, k));
            let c = match idx.stages.get(&(n, k)).map(|&c| w.entity_from_id(c)).filter(|c| c.is_alive()) {
                Some(c) => c,
                None => {
                    let c = w.entity();
                    c.set(StageNo { k });
                    c.child_of(*e.id());
                    idx.stages.insert((n, k), *c.id());
                    c
                }
            };
            set_pair(c, kind, stage_kind);
            set_pair(c, status, tag(s["status"].as_str().unwrap_or("")));
            set_code(c, s["exit_code"].as_i64());
            set_name(c, s["text"].as_str().unwrap_or("").trim());
            set_times(c, s["start_ms"].as_u64(), s["end_ms"].as_u64());
        }
    }
    // Groups sort below every live session (their key past any real timestamp), among themselves by their newest run;
    // "other runs" goes last. Empty ones go.
    idx.groups.retain(|key, g| {
        let g = w.entity_from_id(*g);
        match groups_seen.get(key) {
            Some(&newest) => {
                let created = Created { ms: if key == "other" { u64::MAX } else { (1 << 62) + newest } };
                if g.try_cloned::<&Created>() != Some(created) {
                    g.set(created);
                }
                true
            }
            None => {
                if g.is_alive() {
                    g.destruct();
                }
                false
            }
        }
    });
    // Gone runs go with their stages (ChildOf deletes them).
    idx.runs.retain(|n, e| {
        let keep = seen.contains(n);
        if !keep {
            let e = w.entity_from_id(*e);
            if e.is_alive() {
                e.destruct();
            }
        }
        keep
    });
    idx.stages.retain(|key, c| {
        let keep = seen_stages.contains(key);
        if !keep {
            let c = w.entity_from_id(*c);
            if c.is_alive() {
                c.destruct();
            }
        }
        keep
    });
}

/// Bring the objects in line with the daemon's list: new sessions made, gone ones deleted, each one's components and
/// relations written only where they changed.
fn reconcile(w: WorldRef<'_>, dev: u64, by_id: &mut HashMap<u64, u64>, sessions: &[serde_json::Value]) {
    let mut seen = std::collections::HashSet::new();
    // Two passes: every object exists before any parent is pointed at.
    for s in sessions {
        let Some(id) = s["id"].as_u64() else { continue };
        seen.insert(id);
        let e = match by_id.get(&id).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) {
            Some(e) => e,
            None => {
                let e = w.entity();
                e.set(Object { id });
                by_id.insert(id, *e.id());
                e
            }
        };
        let kind = kind_id(w, s["kind"].as_str().unwrap_or(""));
        if e.target(Kind::id(), 0).is_none_or(|t| *t.id() != kind) {
            e.add((Kind::id(), kind));
        }
        let status = match (s["status"].as_str(), s["exit"].as_i64()) {
            (Some("lost"), _) => *w.component_id::<Lost>(),
            (_, Some(_)) => *w.component_id::<Exited>(),
            (Some("exited"), _) => *w.component_id::<Exited>(),
            _ => *w.component_id::<Running>(),
        };
        if e.target(Status::id(), 0).is_none_or(|t| *t.id() != status) {
            e.add((Status::id(), status));
        }
        if let Some(code) = s["exit"].as_i64() {
            let code = ExitCode { code: code as i32 };
            if e.try_cloned::<&ExitCode>() != Some(code) {
                e.set(code);
            }
        }
        let name = s["name"].as_str().filter(|n| !n.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("{} {id}", s["kind"].as_str().unwrap_or("session")));
        if e.try_cloned::<&Name>().is_none_or(|n| n.s.as_str() != name) {
            e.set(Name { s: name.as_str().into() });
        }
        let title = s["title"].as_str().unwrap_or("");
        if e.try_cloned::<&Title>().is_none_or(|n| n.s.as_str() != title) {
            e.set(Title { s: title.into() });
        }
        let cwd = s["cwd"].as_str().unwrap_or("");
        if e.try_cloned::<&Cwd>().is_none_or(|n| n.s.as_str() != cwd) {
            e.set(Cwd { s: cwd.into() });
        }
        let created = Created { ms: s["created_ms"].as_u64().unwrap_or(0) };
        if e.try_cloned::<&Created>() != Some(created) {
            e.set(created);
        }
    }
    for s in sessions {
        let Some(e) = s["id"].as_u64().and_then(|id| by_id.get(&id)).map(|&e| w.entity_from_id(e)) else { continue };
        let parent = s["parent"].as_u64().and_then(|p| by_id.get(&p).copied()).filter(|&p| p != *e.id()).unwrap_or(dev);
        if e.parent().is_none_or(|p| *p.id() != parent) {
            e.child_of(parent);
        }
    }
    let gone: Vec<u64> = by_id.keys().copied().filter(|id| !seen.contains(id)).collect();
    for id in gone {
        if let Some(e) = by_id.remove(&id) {
            let e = w.entity_from_id(e);
            if e.is_alive() {
                // Its children go back to the device rather than with it.
                let mut kids = Vec::new();
                e.each_child(|c| kids.push(*c.id()));
                for k in kids {
                    w.entity_from_id(k).child_of(dev);
                }
                e.destruct();
            }
        }
    }
}

/// The objects in tree order (depth first, siblings by creation), each with its depth under the device.
#[must_use]
pub fn tree(world: WorldRef<'_>) -> Vec<(u64, usize)> {
    fn walk(w: WorldRef<'_>, e: EntityView<'_>, depth: usize, out: &mut Vec<(u64, usize)>) {
        let mut kids: Vec<(u64, u64)> = Vec::new();
        e.each_child(|c| {
            if c.has(Object::id()) && !gone(c) {
                kids.push((c.try_cloned::<&Created>().map_or(0, |c| c.ms), *c.id()));
            }
        });
        kids.sort_unstable();
        for (_, k) in kids {
            out.push((k, depth));
            walk(w, w.entity_from_id(k), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    let dev = world.lookup(LOCAL);
    if dev.is_alive() && *dev.id() != 0 {
        walk(world, dev, 0, &mut out);
    }
    out
}

/// A row of the sidebar's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A session, run or stage at its depth under the device.
    Obj { e: u64, depth: usize },
    /// `parent`'s folded runs: how many are hidden, or (`unfolded`) the row that folds them again.
    More { parent: u64, hidden: usize, depth: usize, unfolded: bool },
}

/// The sidebar's rows: sessions, their runs and stages in tree order (siblings by start, stages by index). A parent
/// without [`Unfolded`] shows its running runs and its last [`KEEP_RUNS`] ones (and the selected one), the rest behind
/// one [`Row::More`]; a run's stages show while it runs or while it (or one of its stages) is selected.
#[must_use]
pub fn rows(world: WorldRef<'_>) -> Vec<Row> {
    struct Walk<'a> {
        w: WorldRef<'a>,
        sel: Option<u64>,
        sel_parent: Option<u64>,
        running: u64,
        out: Vec<Row>,
    }
    impl Walk<'_> {
        fn is_run(&self, id: u64) -> bool {
            self.w.entity_from_id(id).has(Run::id())
        }
        fn is_running(&self, id: u64) -> bool {
            self.w.entity_from_id(id).target(Status::id(), 0).is_some_and(|t| *t.id() == self.running)
        }
        fn picked(&self, id: u64) -> bool {
            self.sel == Some(id) || self.sel_parent == Some(id)
        }
        fn walk(&mut self, e: EntityView<'_>, depth: usize) {
            let mut kids: Vec<(u64, u64, u64)> = Vec::new();
            e.each_child(|c| {
                if c.target(Kind::id(), 0).is_some() && !gone(c) {
                    let key = match c.try_cloned::<&StageNo>() {
                        Some(s) => u64::from(s.k),
                        None => c.try_cloned::<&Created>().map_or(0, |c| c.ms),
                    };
                    kids.push((key, *c.id(), 0));
                }
            });
            kids.sort_unstable();
            let runs: Vec<u64> = kids.iter().map(|k| k.1).filter(|&id| self.is_run(id)).collect();
            let unfolded = e.has(Unfolded::id());
            let cut = runs.len().saturating_sub(KEEP_RUNS);
            let hidden: std::collections::HashSet<u64> =
                if unfolded { Default::default() } else { runs[..cut].iter().copied().filter(|&r| !self.is_running(r) && !self.picked(r)).collect() };
            let mut more = !hidden.is_empty() || (unfolded && cut > 0);
            for (_, k, _) in kids {
                if more && self.is_run(k) {
                    more = false;
                    self.out.push(Row::More { parent: *e.id(), hidden: if unfolded { cut } else { hidden.len() }, depth, unfolded });
                }
                if hidden.contains(&k) {
                    continue;
                }
                self.out.push(Row::Obj { e: k, depth });
                if ke_collapsed(self.w, k) {
                    continue;
                }
                if !self.is_run(k) || self.is_running(k) || self.picked(k) {
                    let ke = self.w.entity_from_id(k);
                    self.walk(ke, depth + 1);
                }
            }
        }
    }
    let sel = selected(world);
    let mut walk = Walk {
        w: world,
        sel: sel.map(|s| *s.id()),
        sel_parent: sel.and_then(|s| s.parent()).map(|p| *p.id()),
        running: *world.component_id::<Running>(),
        out: Vec::new(),
    };
    let dev = world.lookup(LOCAL);
    if dev.is_alive() && *dev.id() != 0 {
        walk.walk(dev, 0);
    }
    walk.out
}

/// Whether `e` is out of the tree: in the Archive.
fn gone(e: EntityView<'_>) -> bool {
    e.has(Archived::id())
}

/// The objects in the Archive, oldest first.
#[must_use]
pub fn archived(world: WorldRef<'_>) -> Vec<u64> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    world.query::<&Object>().with(Archived::id()).build().each_entity(|e, _| {
        out.push((e.try_cloned::<&Created>().map_or(0, |c| c.ms), *e.id()));
    });
    out.sort_unstable();
    out.into_iter().map(|(_, e)| e).collect()
}

fn ke_collapsed(w: WorldRef<'_>, e: u64) -> bool {
    w.entity_from_id(e).has(Collapsed::id())
}

/// Whether `e` is a group of runs (its row folds it rather than selecting it).
#[must_use]
pub fn is_group(world: WorldRef<'_>, e: u64) -> bool {
    world.entity_from_id(e).has(GroupKey::id())
}

/// How many runs `e` holds directly.
#[must_use]
pub fn run_count(world: WorldRef<'_>, e: u64) -> usize {
    let mut n = 0;
    world.entity_from_id(e).each_child(|c| n += usize::from(c.has(Run::id())));
    n
}

/// Collapse or expand `e`'s children.
pub fn toggle_collapse(world: WorldRef<'_>, e: u64) {
    let e = world.entity_from_id(e);
    if e.has(Collapsed::id()) {
        e.remove(Collapsed::id());
    } else {
        e.add(Collapsed::id());
    }
}

/// Fold or unfold `parent`'s runs.
pub fn toggle_fold(world: WorldRef<'_>, parent: u64) {
    let e = world.entity_from_id(parent);
    if e.has(Unfolded::id()) {
        e.remove(Unfolded::id());
    } else {
        e.add(Unfolded::id());
    }
}

/// The selected object, if any.
#[must_use]
pub fn selected(world: WorldRef<'_>) -> Option<EntityView<'_>> {
    world.lookup(APP).target(Viewing::id(), 0).filter(|e| e.is_alive())
}

/// Select `obj` (or nothing).
pub fn select(world: WorldRef<'_>, obj: Option<u64>) {
    let app = world.lookup(APP);
    match obj {
        Some(o) => {
            app.add((Viewing::id(), o));
        }
        None => {
            app.remove((Viewing::id(), flecs::Wildcard::ID));
        }
    }
}

/// The kind's short name, for the palette and the window title.
#[must_use]
pub fn kind_name(e: EntityView<'_>) -> &'static str {
    let w = e.world();
    let k = e.target(Kind::id(), 0).map_or(0, |t| *t.id());
    [(*w.component_id::<Claude>(), "claude"), (*w.component_id::<Codex>(), "codex"), (*w.component_id::<CursorAgent>(), "cursor"), (*w.component_id::<Shell>(), "shell"), (*w.component_id::<Stage>(), "stage")].into_iter().find(|(id, _)| *id == k).map_or("proc", |(_, n)| n)
}
