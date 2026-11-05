//! harness-native's world: what the daemons hold, as flecs entities, components and relations. The UI is a view over this.
//!
//! - A [`Device`] is a daemon (the tree's root; one today, several later), its link state the exclusive `(Link, Up |
//!   Connecting | Down)`.
//! - An [`Object`] is one of its sessions: `(Kind, Claude | Codex | ...)`, `(Status, Running | Exited | Lost)`, [`Name`],
//!   [`Title`], [`Cwd`], [`ExitCode`] once it ended, and flecs's own `ChildOf` to its parent object or its device.
//! - A shell run (`r12`) is an entity with [`Run`], `(Kind, Proc)`, [`Name`] (its command), `(Status, Running | Exited |
//!   Killed | Lost)`, [`ExitCode`], [`Created`] / [`Ended`], and `ChildOf` its session (or the device). A run split into
//!   pipeline stages has one child per stage: [`StageNo`], `(Kind, Stage)`, its text, its own status, exit code and times.
//! - A run goes under its session, else under the session that owns its REPL (the daemon's `owner`), else on the device.
//!   One whose session or owner the daemon no longer lists is an [`Orphan`].
//! - [`Unfolded`] on a session (or the device) shows all its runs; without it the tree folds all but the last few.
//!   [`Collapsed`] on a node hides all its children (every run group starts so).
//! - [`Archived`] on an object takes it out of the tree and into the Archive pane. The `ArchiveRule` system sets it from
//!   tags and relations alone: [`Closed`] (cmd+W) always, [`Pinned`] never, else a finished object whose parent session
//!   ended or left the tree, an [`Orphan`] run, a finished run on the device [`ArchiveAfter`] it ended, and a finished
//!   top-level [`Past`] session (one from before the daemon started, a chat restored after a restart) `past_ms` after it
//!   ended. A running object and any other finished top-level session stay.
//! - The Archive pane shows the [`ArchiveView`] entity ([`ARCHIVE`]); older runs come a page at a time ([`Paged`],
//!   [`History`]) rather than all at once.
//! - `(Selected, object)` on the app entity is the selection (exclusive). Each pane node of the view holds the exclusive
//!   `(Shows, object)`: what it draws. The focused pane's `Shows` follows `Selected`; a second pane is a second node.
//! - An attached object has [`Term`]: its grid's size and how far it is scrolled back; the terminal (harness-vt) itself is the
//!   host's, in [`Terms`] by session id (a parser is no plain data a component can hold).
//!
//! The daemon link (a thread in the app) posts [`Msg`]s to a [`Mailbox`]; the `DaemonInbox` system applies them to the
//! world each frame.
pub use harness_chat as chat;

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

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
/// A daemon took the connection but the app cannot speak to it (no hello in time: an older daemon; another schema). The
/// device's [`LinkError`] says which daemon and what to do.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Refused;

/// On the device while its link is [`Refused`]: the link's error, naming the daemon's pid and socket and `harness stop`.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct LinkError {
    pub text: ScriptString,
}

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
/// A run or a session ended by a signal.
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

/// On an object: the user closed it (cmd+W, Kill); it is archived whatever else holds. The host keeps these.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Closed;

/// On an object: "Keep in tree" (or Unarchive): the rule never archives it. The host keeps these.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pinned;

/// On a session: it ended before the daemon started (the daemon's `past`: a chat from before a restart, listed from its
/// index). The rule treats it as any finished session of this daemon's, except that a top-level one leaves the tree for
/// the Archive [`ArchiveAfter`]'s `past_ms` after it ended; resumed, it runs again where it is.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Past;

/// On a run: it names a session (its own or its REPL's owner) that the daemon no longer lists.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Orphan;

/// On a session: its program wrote output lately (the daemon's `busy`; a daemon that does not say counts as busy), so
/// its running dot breathes. A live shell sitting at its prompt is not busy, and draws nothing.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Busy;

/// On a session: it runs in chat mode (the daemon's `mode`): its agent speaks structured events, not a terminal, and a chat
/// pane draws it from its [`Chats`] transcript.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ChatMode;

/// On a chat session: the model its agent said it runs (the daemon's `model`).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct AgentModel {
    pub s: ScriptString,
}

/// On a run: it came from a page of older history (the Archive's), so the poll's window does not drop it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Paged;

/// `(Kind, ArchiveView)`: the Archive pane's entity, selected and shown like an object.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ArchiveView;

/// The Archive pane's entity.
pub const ARCHIVE: &str = "views::archive";

/// Singleton: how long after they ended finished objects with nothing above them leave the tree (the scene's tokens).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ArchiveAfter {
    /// A finished run on the device (no session).
    pub ms: u64,
    /// A finished top-level [`Past`] session (one from before the daemon started).
    pub past_ms: u64,
}

impl ArchiveAfter {
    /// Ten minutes for a run, a week for a past session.
    pub const DEFAULT: Self = Self { ms: 600_000, past_ms: 7 * 24 * 3_600_000 };
}

/// Singleton: the older history's paging: a page asked for and not in yet, and whether the daemon has no older runs.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct History {
    pub loading: bool,
    pub exhausted: bool,
}

/// Finished runs a parent shows while folded (running ones always show).
pub const KEEP_RUNS: usize = 3;

/// When it was made (ms since the epoch): the tree's order among siblings.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Created {
    pub ms: u64,
}

/// `(Selected, object)` on the app entity: the selected object (exclusive). The focused pane shows it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Selected;

/// `(Shows, object)` on a pane node of the view: the object that pane draws (exclusive). Every pane draws from its own.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Shows;

/// Which pane prefab draws an object, by its Kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    /// A terminal (Claude, Codex, CursorAgent, Shell, and a session of a kind the model does not know).
    Term,
    /// A run's or stage's output (Proc, Stage).
    Run,
    /// The Archive (ArchiveView).
    Archive,
    /// A chat session's transcript and its input ([`ChatMode`]).
    Chat,
}

/// The pane kind for `e`. A session whose kind the daemon named something unknown is `(Kind, Proc)` too, but it is a
/// terminal: Proc picks the run pane only for a run.
#[must_use]
pub fn pane_kind(e: EntityView<'_>) -> PaneKind {
    let w = e.world();
    let k = e.target(Kind::id(), 0).map_or(0, |t| *t.id());
    if k == *w.component_id::<ArchiveView>() {
        PaneKind::Archive
    } else if e.has(ChatMode::id()) {
        PaneKind::Chat
    } else if k == *w.component_id::<Stage>() || (k == *w.component_id::<Proc>() && !e.has(Object::id())) {
        PaneKind::Run
    } else {
        PaneKind::Term
    }
}

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

/// The transcripts of attached chat sessions, by session id.
pub type Chats = Rc<RefCell<HashMap<u64, chat::Transcript>>>;

/// Rows of scrollback a terminal keeps.
pub const SCROLLBACK: usize = 10_000;

/// What the daemon link sends the world.
#[derive(Debug)]
pub enum Msg {
    Link(LinkState),
    /// The daemon took the connection but the app cannot speak to it: the link is [`Refused`] for this reason.
    Refused(String),
    /// Every session the daemon has, as its `list` op returns them.
    List(Vec<serde_json::Value>),
    /// The attach stream's bytes (the snapshot first).
    Data { id: u64, bytes: Vec<u8> },
    Exit { id: u64, code: i32 },
    /// The attach stream ended.
    Detached { id: u64 },
    /// The daemon's recent shell runs, as its `runs` op returns them (newest first).
    Runs(Vec<serde_json::Value>),
    /// A page of older runs (`runs` with `before`), and whether it was a full page (more may be older).
    RunsPage { runs: Vec<serde_json::Value>, full: bool },
    /// Runs from all of history matching a search (`runs` with `q`, the palette's): added like a page, paging untouched.
    RunsFound(Vec<serde_json::Value>),
    /// Output of run `r{run}` (of its stage `stage`, or the caller-visible streams), as `run_read` returned it.
    RunData { run: u64, stage: Option<u32>, err: bool, bytes: Vec<u8> },
}

/// Where the daemon link's threads post [`Msg`]s for the `DaemonInbox` system to take, in the order posted, except that
/// a [`Msg::List`] or [`Msg::Runs`] is the whole tree as the daemon tells it: a new one replaces the one still waiting
/// (in its place), since applying the older first would change nothing the newer does not. So however long nothing
/// takes (a window the display link stopped drawing), at most one of each waits, not one per change the daemon
/// announced (up to 20 a second, each a copy of every session and the last 100 runs).
#[derive(Clone, Default)]
pub struct Mailbox(Arc<Mutex<Waiting>>);

#[derive(Default)]
struct Waiting {
    msgs: VecDeque<Msg>,
    /// Where in `msgs` the waiting `List` and `Runs` are.
    list: Option<usize>,
    runs: Option<usize>,
}

impl Mailbox {
    pub fn send(&self, msg: Msg) {
        let mut w = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = match msg {
            Msg::List(_) => w.list,
            Msg::Runs(_) => w.runs,
            _ => None,
        };
        if let Some(i) = slot {
            w.msgs[i] = msg;
            return;
        }
        let i = w.msgs.len();
        match msg {
            Msg::List(_) => w.list = Some(i),
            Msg::Runs(_) => w.runs = Some(i),
            _ => {}
        }
        w.msgs.push_back(msg);
    }

    /// Every message waiting, oldest first.
    pub fn take(&self) -> VecDeque<Msg> {
        let mut w = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (w.list, w.runs) = (None, None);
        std::mem::take(&mut w.msgs)
    }

    /// How many messages wait.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).msgs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Up,
    Connecting,
    Down,
    /// See [`Refused`]; sent as [`Msg::Refused`] with its reason.
    Refused,
}

pub fn register(world: &World) {
    world.component::<Device>().writable();
    world.component::<LinkError>().writable();
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
    world.component::<Closed>();
    world.component::<Pinned>();
    world.component::<Orphan>();
    world.component::<Busy>();
    world.component::<Past>();
    world.component::<ChatMode>();
    world.component::<AgentModel>().writable();
    world.component::<Paged>();
    world.component::<ArchiveView>();
    world.component::<ArchiveAfter>().writable();
    world.component::<History>().writable();
    world.set(ArchiveAfter::DEFAULT);
    world.set(History::default());
    world.component::<Selected>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Shows>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Term>().writable();
    world.component::<Inbox>().writable();
    world.set(Inbox::default());
}

fn for_tags(world: &World) {
    world.component::<Up>();
    world.component::<Connecting>();
    world.component::<Down>();
    world.component::<Refused>();
    world.component::<Claude>();
    world.component::<Codex>();
    world.component::<CursorAgent>();
    world.component::<Shell>();
    world.component::<Proc>();
    world.component::<Stage>();
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

/// Make the local device and install the inbox: each frame it drains `mail` into the world. Returns the terminal parsers:
/// the sessions' (by session id) and the runs' and stages' (by entity id), and the chat sessions' transcripts.
pub fn install(world: WorldRef<'_>, host: &str, sock: &str, mail: Mailbox) -> (Terms, Terms, Chats) {
    let dev = world.entity_named(LOCAL);
    dev.set(Device { host: host.into(), sock: sock.into() });
    dev.add((Link::id(), Connecting::id()));
    let dev = *dev.id();
    let terms: Terms = Rc::default();
    let run_terms: Terms = Rc::default();
    let chats: Chats = Rc::default();
    let parsers = (terms.clone(), run_terms.clone(), chats.clone());
    let view = world.entity_named(ARCHIVE);
    view.add((Kind::id(), *world.component_id::<ArchiveView>()));
    view.set(Name { s: "Archive".into() });
    install_rule(world);
    let mut idx = Index::default();
    // The last list and runs answers applied, as fingerprints of what reconciling reads: an answer that changes none of
    // it is dropped, so a daemon telling the same tree again costs no frame.
    let (mut last_list, mut last_runs) = (None::<u64>, None::<u64>);
    world.system_named::<&mut Inbox>("DaemonInbox").each_iter(move |it, _, inbox| {
        let w = it.world();
        // Consecutive output for one session goes to its parser as one feed (the daemon sends ~1 KB frames).
        let mut data: Option<(u64, Vec<u8>)> = None;
        let flush = |data: &mut Option<(u64, Vec<u8>)>, idx: &mut Index, inbox: &mut Inbox| {
            if let Some((id, bytes)) = data.take() {
                inbox.applied = inbox.applied.wrapping_add(1);
                apply(w, dev, idx, &parsers, Msg::Data { id, bytes });
            }
        };
        for msg in mail.take() {
            match msg {
                Msg::Data { id, bytes } => {
                    match &mut data {
                        Some((d, buf)) if *d == id => buf.extend_from_slice(&bytes),
                        _ => {
                            flush(&mut data, &mut idx, inbox);
                            data = Some((id, bytes));
                        }
                    }
                    continue;
                }
                Msg::List(ref s) => {
                    let f = fingerprint(s, LIST_KEYS, &[]);
                    if last_list == Some(f) {
                        continue;
                    }
                    last_list = Some(f);
                    // Runs hang under sessions: a changed list reconciles the next runs answer too.
                    last_runs = None;
                }
                Msg::Runs(ref r) => {
                    let f = fingerprint(r, RUN_KEYS, STAGE_KEYS);
                    if last_runs == Some(f) {
                        continue;
                    }
                    last_runs = Some(f);
                }
                Msg::Link(_) => (last_list, last_runs) = (None, None),
                _ => {}
            }
            flush(&mut data, &mut idx, inbox);
            inbox.applied = inbox.applied.wrapping_add(1);
            apply(w, dev, &mut idx, &parsers, msg);
        }
        flush(&mut data, &mut idx, inbox);
    });
    (terms, run_terms, chats)
}

/// The session fields [`reconcile`] reads.
const LIST_KEYS: &[&str] = &["id", "kind", "status", "exit", "name", "title", "cwd", "created_ms", "ended_ms", "parent", "busy", "mode", "model", "past"];
/// The run fields [`reconcile_runs`] reads, and each stage's.
const RUN_KEYS: &[&str] = &["id", "status", "exit_code", "cmd", "cwd", "start_ms", "end_ms", "session", "owner"];
const STAGE_KEYS: &[&str] = &["status", "exit_code", "text", "start_ms", "end_ms"];

/// A hash of `keys` of each item (and `stage_keys` of each of its `stages`), in order.
fn fingerprint(items: &[serde_json::Value], keys: &[&str], stage_keys: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let field = |h: &mut std::collections::hash_map::DefaultHasher, v: &serde_json::Value, keys: &[&str]| {
        for k in keys {
            v[*k].to_string().hash(h);
        }
    };
    for v in items {
        field(&mut h, v, keys);
        if !stage_keys.is_empty() {
            for s in v["stages"].as_array().into_iter().flatten() {
                field(&mut h, s, stage_keys);
            }
        }
        0xffu8.hash(&mut h);
    }
    h.finish()
}

/// Milliseconds since the epoch.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The `ArchiveRule` system: each session and run (not a stage) gets [`Archived`] exactly when the rule holds.
/// - [`Closed`]: archived. [`Pinned`]: kept.
/// - Running: kept.
/// - Finished, under a session: archived once that session ended or left the tree (its runs go with it).
/// - A finished run that is an [`Orphan`]: archived; one on the device: archived [`ArchiveAfter`] it ended.
/// - A finished top-level [`Past`] session (it ended before the daemon started): archived [`ArchiveAfter`]'s `past_ms`
///   after it ended (so a restart leaves recent chats in the tree, dimmed by their status like any finished session).
/// - Any other finished top-level session: kept until it is closed.
///
/// One level a frame: a session archived this frame takes its runs with it the next.
fn install_rule(world: WorldRef<'_>) {
    let running = *world.component_id::<Running>();
    world.system_named::<&Created>("ArchiveRule").with((Status::id(), flecs::Wildcard::ID)).without(StageNo::id()).each_entity(move |e, _| {
        if !e.has(Object::id()) && !e.has(Run::id()) {
            return;
        }
        let is_running = |x: EntityView<'_>| x.target(Status::id(), 0).is_some_and(|t| *t.id() == running);
        let want = if e.has(Closed::id()) {
            true
        } else if e.has(Pinned::id()) || is_running(e) {
            false
        } else {
            let after = e.world().try_get::<&ArchiveAfter>(|a| *a).unwrap_or(ArchiveAfter::DEFAULT);
            let ended = || e.try_cloned::<&Ended>().map(|t| t.ms).or_else(|| e.try_cloned::<&Created>().map(|c| c.ms)).unwrap_or(0);
            match e.parent().filter(|p| p.has(Object::id())) {
                Some(p) => p.has(Archived::id()) || !is_running(p),
                None if e.has(Run::id()) => e.has(Orphan::id()) || ended().saturating_add(after.ms) <= now_ms(),
                None if e.has(Past::id()) => ended().saturating_add(after.past_ms) <= now_ms(),
                None => false,
            }
        };
        if e.has(Archived::id()) != want {
            if want {
                e.add(Archived::id());
            } else {
                e.remove(Archived::id());
            }
        }
    });
}

/// Daemon ids to entities: sessions by id, runs by number, stages by (run number, index).
#[derive(Default)]
struct Index {
    sessions: HashMap<u64, u64>,
    runs: HashMap<u64, u64>,
    stages: HashMap<(u64, u32), u64>,
}

fn apply(w: WorldRef<'_>, dev: u64, idx: &mut Index, (terms, run_terms, chats): &(Terms, Terms, Chats), msg: Msg) {
    let by_id = &mut idx.sessions;
    match msg {
        Msg::Link(s) => {
            let tag = match s {
                LinkState::Up => *w.component_id::<Up>(),
                LinkState::Connecting => *w.component_id::<Connecting>(),
                LinkState::Down => *w.component_id::<Down>(),
                LinkState::Refused => *w.component_id::<Refused>(),
            };
            let d = w.entity_from_id(dev);
            d.add((Link::id(), tag));
            if s != LinkState::Refused && d.has(LinkError::id()) {
                d.remove(LinkError::id());
            }
        }
        Msg::Refused(why) => {
            let d = w.entity_from_id(dev);
            d.add((Link::id(), Refused::id()));
            d.set(LinkError { text: why.as_str().into() });
        }
        Msg::List(sessions) => reconcile(w, dev, by_id, &sessions),
        Msg::Data { id, bytes } => {
            let Some(e) = by_id.get(&id).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) else { return };
            if e.has(ChatMode::id()) {
                chats.borrow_mut().entry(id).or_default().feed(&bytes);
                return;
            }
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
        Msg::Runs(runs) => reconcile_runs(w, dev, idx, &runs, false),
        Msg::RunsPage { runs, full } => {
            reconcile_runs(w, dev, idx, &runs, true);
            w.set(History { loading: false, exhausted: !full });
        }
        Msg::RunsFound(runs) => reconcile_runs(w, dev, idx, &runs, true),
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

/// Bring the runs in line with the daemon's `runs` answer: each a child of its session (else its REPL's owner, else the
/// device), its stages children of it when it was split into more than one. A `paged` answer (older history) only adds:
/// runs it does not name stay, and the runs it brings are [`Paged`], kept when the poll's window does not name them.
fn reconcile_runs(w: WorldRef<'_>, dev: u64, idx: &mut Index, runs: &[serde_json::Value], paged: bool) {
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
    let mut seen = std::collections::HashSet::new();
    let mut seen_stages = std::collections::HashSet::new();
    for r in runs {
        let Some(n) = r["id"].as_str().and_then(|s| s.strip_prefix('r')).and_then(|s| s.parse::<u64>().ok()) else { continue };
        seen.insert(n);
        let e = match idx.runs.get(&n).map(|&e| w.entity_from_id(e)).filter(|e| e.is_alive()) {
            Some(e) => e,
            None => {
                let e = w.entity();
                e.set(Run { n });
                if paged {
                    e.add(Paged::id());
                }
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
        // Its session, else the session its REPL's cells ran under; a named one the daemon no longer lists orphans it.
        let named = r["session"].as_u64().or_else(|| r["owner"].as_u64());
        let listed = named.and_then(|s| idx.sessions.get(&s).copied()).filter(|&p| w.entity_from_id(p).is_alive());
        let parent = listed.unwrap_or(dev);
        if (named.is_some() && listed.is_none()) != e.has(Orphan::id()) {
            if listed.is_none() {
                e.add(Orphan::id());
            } else {
                e.remove(Orphan::id());
            }
        }
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
    // Gone runs go with their stages (ChildOf deletes them).
    if paged {
        return;
    }
    idx.runs.retain(|n, e| {
        let keep = seen.contains(n) || w.entity_from_id(*e).has(Paged::id());
        if !keep {
            let e = w.entity_from_id(*e);
            if e.is_alive() {
                e.destruct();
            }
        }
        keep
    });
    idx.stages.retain(|key, c| {
        let keep = seen_stages.contains(key) || (idx.runs.contains_key(&key.0) && !seen.contains(&key.0));
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
            (Some("killed"), _) => *w.component_id::<Killed>(),
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
        if (s["mode"] == "chat") != e.has(ChatMode::id()) {
            if s["mode"] == "chat" {
                e.add(ChatMode::id());
            } else {
                e.remove(ChatMode::id());
            }
        }
        let model = s["model"].as_str().unwrap_or("");
        if e.try_cloned::<&AgentModel>().is_none_or(|m| m.s.as_str() != model) {
            e.set(AgentModel { s: model.into() });
        }
        let past = s["past"].as_bool().unwrap_or(false);
        if e.has(Past::id()) != past {
            if past {
                e.add(Past::id());
            } else {
                e.remove(Past::id());
            }
        }
        let busy = s["busy"].as_bool().unwrap_or(true);
        if e.has(Busy::id()) != busy {
            if busy {
                e.add(Busy::id());
            } else {
                e.remove(Busy::id());
            }
        }
        // When it ended (the daemon's `ended_ms`): where the Archive files it.
        set_times(e, s["created_ms"].as_u64(), s["ended_ms"].as_u64());
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
        for c in live_kids(e) {
            if c.has(Object::id()) {
                kids.push((c.try_cloned::<&Created>().map_or(0, |c| c.ms), *c.id()));
            }
        }
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
            for c in live_kids(e) {
                let key = match c.try_cloned::<&StageNo>() {
                    Some(s) => u64::from(s.k),
                    None => c.try_cloned::<&Created>().map_or(0, |c| c.ms),
                };
                kids.push((key, *c.id(), 0));
            }
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

/// `e`'s children in the tree: those with a Kind that are not archived, and in place of an archived one, what under it
/// is still in the tree (a run still running under a closed session floats up to where its session was).
fn live_kids(e: EntityView<'_>) -> Vec<EntityView<'_>> {
    let mut out = Vec::new();
    e.each_child(|c| {
        if c.target(Kind::id(), 0).is_none() {
            return;
        }
        if c.has(Archived::id()) {
            out.extend(live_kids(c).into_iter().filter(|k| !k.has(StageNo::id())));
        } else {
            out.push(c);
        }
    });
    out
}

/// The sessions and runs in the Archive (not stages), oldest first by when they ended (else began).
#[must_use]
pub fn archived(world: WorldRef<'_>) -> Vec<u64> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    world.query::<&Created>().with(Archived::id()).build().each_entity(|e, c| {
        if e.has(Object::id()) || e.has(Run::id()) {
            out.push((e.try_cloned::<&Ended>().map_or(c.ms, |t| t.ms), *e.id()));
        }
    });
    out.sort_unstable();
    out.into_iter().map(|(_, e)| e).collect()
}

/// When `e` ended, else when it began (ms since the epoch): where the Archive files it.
#[must_use]
pub fn when(e: EntityView<'_>) -> u64 {
    e.try_cloned::<&Ended>().map(|t| t.ms).or_else(|| e.try_cloned::<&Created>().map(|c| c.ms)).unwrap_or(0)
}

/// The Archive pane's entity.
#[must_use]
pub fn archive_view(world: WorldRef<'_>) -> u64 {
    *world.lookup(ARCHIVE).id()
}

fn ke_collapsed(w: WorldRef<'_>, e: u64) -> bool {
    w.entity_from_id(e).has(Collapsed::id())
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
    world.lookup(APP).target(Selected::id(), 0).filter(|e| e.is_alive())
}

/// What pane node `pane` shows, if anything.
#[must_use]
pub fn shows(world: WorldRef<'_>, pane: u64) -> Option<EntityView<'_>> {
    world.entity_from_id(pane).target(Shows::id(), 0).filter(|e| e.is_alive())
}

/// Point pane node `pane` at `obj` (or at nothing).
pub fn show(world: WorldRef<'_>, pane: u64, obj: Option<u64>) {
    let p = world.entity_from_id(pane);
    match obj {
        Some(o) if p.target(Shows::id(), 0).is_none_or(|t| *t.id() != o) => {
            p.add((Shows::id(), o));
        }
        None if p.target(Shows::id(), 0).is_some() => {
            p.remove((Shows::id(), flecs::Wildcard::ID));
        }
        _ => {}
    }
}

/// Select `obj` (or nothing).
pub fn select(world: WorldRef<'_>, obj: Option<u64>) {
    let app = world.lookup(APP);
    match obj {
        Some(o) => {
            app.add((Selected::id(), o));
        }
        None => {
            app.remove((Selected::id(), flecs::Wildcard::ID));
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

