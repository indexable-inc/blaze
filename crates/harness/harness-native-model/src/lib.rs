//! harness-native's world: what the daemons hold, as flecs entities, components and relations. The UI is a view over this.
//!
//! - A [`Device`] is a daemon (the tree's root; one today, several later), its link state the exclusive `(Link, Up |
//!   Connecting | Down)`.
//! - An [`Object`] is one of its sessions: `(Kind, Claude | Codex | ...)`, `(Status, Running | Exited | Lost)`, [`Name`],
//!   [`Title`], [`Cwd`], [`ExitCode`] once it ended, and flecs's own `ChildOf` to its parent object or its device.
//! - `(Viewing, object)` on the app entity is what the main pane shows (exclusive).
//! - An attached object has [`Term`]: its grid's size and how far it is scrolled back; the vt100 parser itself is the
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

/// `(Status, status)`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Status;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Running;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Exited;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Lost;

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

/// The vt100 parsers of attached objects, by session id.
pub type Terms = Rc<RefCell<HashMap<u64, vt100::Parser>>>;

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
    world.component::<Running>();
    world.component::<Exited>();
    world.component::<Lost>();
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

/// Make the local device and install the inbox: each frame it drains `rx` into the world. Returns the terminal parsers.
pub fn install(world: WorldRef<'_>, host: &str, sock: &str, rx: Receiver<Msg>) -> Terms {
    let dev = world.entity_named(LOCAL);
    dev.set(Device { host: host.into(), sock: sock.into() });
    dev.add((Link::id(), Connecting::id()));
    let dev = *dev.id();
    let terms: Terms = Rc::default();
    let parsers = terms.clone();
    let mut by_id: HashMap<u64, u64> = HashMap::new();
    world.system_named::<&mut Inbox>("DaemonInbox").each_iter(move |it, _, inbox| {
        let w = it.world();
        for msg in rx.try_iter() {
            inbox.applied = inbox.applied.wrapping_add(1);
            apply(w, dev, &mut by_id, &parsers, msg);
        }
    });
    terms
}

fn apply(w: WorldRef<'_>, dev: u64, by_id: &mut HashMap<u64, u64>, terms: &Terms, msg: Msg) {
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
            let p = map.entry(id).or_insert_with(|| vt100::Parser::new(nz(t.rows), nz(t.cols), SCROLLBACK));
            p.process(&bytes);
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
    }
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
            if c.has(Object::id()) {
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
