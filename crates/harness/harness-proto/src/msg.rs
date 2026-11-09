//! The messages of a [`crate::frame::T_CBOR`] frame: [`Up`] from a client, [`Down`] from the daemon.
//!
//! A connection starts with the client's [`Up::Hello`]; the client may send its first request right behind it without
//! waiting. The daemon answers [`Down::Hello`] (or [`Down::Error`] naming both versions, and closes), then one [`Down`]
//! per request: [`Down::Reply`] or [`Down::Error`]. An `attach` or `watch` request turns the connection into a stream:
//! after its reply come terminal bytes ([`crate::frame::T_DATA`]) or chat events ([`crate::frame::T_EVENT`]),
//! [`Down::Exit`] when the session's process ends, [`Down::Changed`] on a watch; the client sends keys
//! ([`crate::frame::T_DATA`]), [`Up::Resize`], and [`Up::Detach`] to end it. A `ui_listen` request (the kernel's) turns
//! it into the feed of what the person does to the UI its cells drew: a [`Down::Ui`] per [`Request::UiEvent`] the app
//! sends for one of its cells.
//!
//! Each map is tagged by `t` (and a request by `op`, a reply by `reply`), so the kernel reads a reply as one flat map.
//! The parts whose shape another program owns stay open records ([`Value`]): a REPL cell as the kernel posts it, a tool
//! call's input, an object API call's arguments and a run's or an object's row (crates/harness/harness/src/runs.rs,
//! object.rs and db.rs define those).
use crate::event::ModelChoice;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A client's message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Up {
    /// The first message on every connection: the schema the client speaks.
    Hello { schema: u32 },
    /// A request; one [`Down`] answers it.
    Req(Request),
    /// On an attach stream: the client's terminal is this size now.
    Resize { cols: u16, rows: u16 },
    /// On an attach or watch stream: end it (the session keeps running).
    Detach,
}

/// The daemon's message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Down {
    /// The answer to [`Up::Hello`]: the schema this daemon speaks, and its pid.
    Hello { schema: u32, pid: u32 },
    /// A request's answer.
    Reply(Reply),
    /// A request refused, or the hello's schema not this daemon's: why, naming the thing.
    Error { error: String },
    /// On an attach stream: the session's process exited with this code.
    Exit { code: i32 },
    /// On a watch stream: what `list` or `runs` answers changed; the tree's generation now.
    Changed {
        #[serde(rename = "gen")]
        generation: u64,
    },
    /// On a `ui_listen` stream: the person did something to cell `cell`'s UI; run its handler `handler` with `value`
    /// (the [`Request::UiEvent`] as the app sent it).
    Ui {
        cell: String,
        handler: String,
        #[serde(default)]
        value: Value,
    },
}

/// A request (the `op` of an [`Up::Req`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Live sessions; with `all` also past ones from the index (status exited | lost), with `chats` every past chat.
    /// Each row says `past`: it ended before this daemon started.
    List {
        #[serde(default)]
        all: bool,
        #[serde(default)]
        chats: bool,
    },
    /// The daemon's pid and socket.
    Info,
    /// The session last attached to, while it runs.
    Last,
    Spawn {
        kind: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        parent: Option<u64>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        /// `chat` (Claude's and Codex's default) or `tui` (a PTY).
        #[serde(default)]
        mode: Option<String>,
        /// The model the agent starts on (a [`crate::event::ModelChoice::id`] its kind listed) and its reasoning effort:
        /// passed to the agent's launch, so it runs with them from its first turn. A model or effort its kind's list
        /// does not have is refused, naming it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// Hold the connection as session `id`'s stream. `events`: a chat session's events as [`crate::frame::T_EVENT`]
    /// frames (the app) instead of the conversation as text (the terminal client).
    Attach {
        id: u64,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        events: bool,
    },
    /// The change feed: a [`Reply::Watching`], then a [`Down::Changed`] each time `list` or `runs` would answer
    /// differently, coalesced to at most 20 a second, until the client sends anything.
    Watch,
    /// Type into a session. `submit` (default true) presses Enter afterwards; text goes in as a bracketed paste so
    /// multi-line messages arrive as one input. `raw` writes bytes verbatim. A chat session takes it as a message, with
    /// `images`.
    Send {
        id: u64,
        text: String,
        #[serde(default)]
        raw: bool,
        #[serde(default)]
        submit: Option<bool>,
        /// Pictures sent with a chat message (absolute paths of PNG or JPEG files): the agent gets each as an image.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// Switch chat session `id`'s model (an id from its `models` event, else from the list its agent kind last gave,
    /// [`Reply::Sessions`]'s `models`) and/or reasoning effort (one that model takes) for its turns from here on. The
    /// daemon checks both against that list, records the choice in the chat's log (a `choice` event, so a resume keeps
    /// it) and, while the agent runs, asks it: claude over its control requests (`set_model`, `apply_flag_settings`),
    /// codex on its next `turn/start`; a chat whose agent is not running only records it, and its resume starts with it.
    /// Refused, naming the session and the value, when no agent of its kind has listed models yet or the list lacks it.
    SetModel {
        id: u64,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
    },
    /// Name session `id` (its [`SessionInfo::label`], what the app shows for it): `title` trimmed; empty takes the name
    /// away, and the session is called what it was before (its `name`, else its kind and number). The daemon keeps it in
    /// the session's index row, so it outlives the daemon and the app. Refused, naming the session, for a title over a
    /// line.
    Rename { id: u64, title: String },
    /// A "by the way" question about chat session `id`'s conversation that stays out of it: the daemon forks the
    /// conversation (claude's `--resume --fork-session`, never saved; codex's ephemeral `thread/fork`), runs `text` as
    /// one turn on the fork with the chat's model and effort, and discards the fork. Answered by [`Reply::Aside`] once
    /// the turn ends, within [`ASIDE_SECS`] (the fork is stopped past it); refused, naming the chat and why, when it is
    /// not a chat, its log never named the agent's id for the conversation, or the fork failed or did not answer. The
    /// chat's log and its agent are untouched.
    Aside { id: u64, text: String },
    Screen { id: u64 },
    Resize { id: u64, cols: u16, rows: u16 },
    Kill { id: u64 },
    Forget { id: u64 },
    Shutdown,
    /// A REPL cell started or finished (posted by the kernel): indexed as a `repl` object; the cell as the kernel shapes
    /// it (mcp/src/tasks.ts).
    ReplTask {
        #[serde(flatten)]
        task: Value,
    },
    /// The person did something to a cell's UI (the app sends it): cell `cell` (`<repl>.<task>`, a tool detail's `cell`)
    /// runs handler `handler` (the id the UI's node carries, `h3`) with `value` (a toggle's new state, a slider's number,
    /// a field's text; null for a press). The daemon hands it to the kernel listening for `<repl>`, or refuses naming the
    /// cell when none is.
    UiEvent {
        cell: String,
        handler: String,
        #[serde(default)]
        value: Value,
    },
    /// Hold the connection as kernel `repl`'s UI feed (the kernel sends it): a [`Reply::Listening`], then a [`Down::Ui`]
    /// per [`Request::UiEvent`] for one of its cells, until the client goes. A newer listener for the same `repl`
    /// replaces this one.
    UiListen { repl: String },
    /// The object API (crates/harness/harness/src/object.rs): get-or-create any kind, one object's record, and RPC to it.
    SpawnObject {
        #[serde(flatten)]
        opts: Value,
    },
    Object { id: String },
    Rpc {
        id: String,
        method: String,
        #[serde(default)]
        args: Value,
    },
    /// The embedded browser (crates/harness/harness-browser): start this daemon's host if it is not running, and say
    /// where it listens. Answered by [`Reply::Browser`] once the host is ready, or an error naming what failed (its app
    /// bundle missing, CEF failing to start), never a stand-in browser.
    Browser,
    /// The state of the pull requests and issues a chat's words name (the app's chips, harness-proto's peek.rs): one
    /// batched GitHub query for all of them, as `gh` is signed in. Answered from a short-lived cache unless `fresh` (the
    /// chat was just focused). At most [`crate::peek::REFS_MAX`] refs; each comes back as a [`crate::peek::GhItem`],
    /// with an error naming it when it could not be read.
    GithubStatus {
        refs: Vec<crate::peek::GhRef>,
        #[serde(default)]
        fresh: bool,
    },
    /// A pull request's head commit's checks one by one (`pr`, a pull request's ref): each one's state, the step in
    /// progress of one that runs, when it started and ended, and its page, at most [`crate::peek::CHECKS_MAX`]. The app
    /// asks while the pull request's hover card shows and a check is unfinished; answered from a cache of a few seconds.
    /// A failure is a [`crate::peek::GhChecks`] whose error names the pull request.
    GithubChecks { pr: crate::peek::GhRef },
    /// A web page's card for a hover (`url`, http or https): its OpenGraph title, description, site and image, fetched
    /// once and kept.
    PagePeek { url: String },
    /// Shell runs with every pipeline stage's output stored (crates/harness/harness/src/runs.rs).
    #[serde(untagged)]
    Runs(RunReq),
}

/// A shell run request (crates/harness/harness/src/runs.rs answers each).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RunReq {
    /// Start a command; answers `{id}` at once.
    Run {
        cmd: String,
        #[serde(default)]
        cwd: Option<String>,
        /// Variables set on top of the daemon's environment (or instead of it with `clear_env`).
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        clear_env: bool,
        #[serde(default)]
        session: Option<u64>,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        stdin: Option<String>,
        /// The REPL (kernel) that started it, when it has no session; kept in meta as `repl`.
        #[serde(default)]
        repl: Option<String>,
    },
    RunStatus { id: String },
    /// The newest runs (of `session`), up to `limit`; with `before`, only runs numbered below it (the next page back);
    /// with `q`, only runs whose command or id (`r12`) holds it, case-insensitively (a search over all history).
    Runs {
        #[serde(default)]
        session: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        before: Option<u64>,
        #[serde(default)]
        q: Option<String>,
    },
    /// `stage: None` reads the caller-visible streams (out = "final" stdout, err = final.err).
    RunRead {
        id: String,
        #[serde(default)]
        stage: Option<usize>,
        #[serde(default = "default_stream")]
        stream: String,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        max_bytes: Option<usize>,
        /// Long-poll: wait up to this long for new data when there's none yet.
        #[serde(default)]
        wait_ms: u64,
    },
    RunKill { id: String },
    RunWait {
        id: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Write to a live run's stdin (runs started with `live_stdin`); `close` ends it (EOF).
    RunStdin {
        id: String,
        #[serde(default)]
        data: Option<String>,
        #[serde(default)]
        close: bool,
    },
}

/// How long an aside's fork may take to answer ([`Request::Aside`]): the daemon stops it past this, and a client waits
/// this long and a little more for the reply.
pub const ASIDE_SECS: u64 = 600;

fn default_stream() -> String {
    "out".into()
}

/// A request's answer (the `reply` of a [`Down::Reply`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// Done; nothing to say.
    Ok,
    /// `spawn`: the new session.
    Spawned { id: u64 },
    /// `list`. `models`: by agent kind (`claude`, `codex`), the models that kind last listed to this daemon (kept across
    /// its restarts), what a chat whose own log has no `models` event picks from; a kind that never listed is absent.
    Sessions {
        sessions: Vec<SessionInfo>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        models: BTreeMap<String, Vec<ModelChoice>>,
    },
    /// `info`.
    Daemon { pid: u32, sock: String },
    /// `last`: the session last attached to, if it still runs.
    Last { id: Option<u64> },
    /// `screen`: its text and its row.
    Screen { text: String, info: SessionInfo },
    /// `attach`: the stream follows.
    Attached { info: SessionInfo },
    /// `watch`: the change feed follows.
    Watching,
    /// `ui_listen`: the UI feed follows.
    Listening,
    /// `browser`: the host's pid, its socket for the kernel (`$HARNESS_HOME/browser/host.sock`) and the Mach service the
    /// app looks up (crate::browser).
    Browser { pid: u32, sock: String, service: String },
    /// `github_status`: one item a ref asked, in its order.
    Github { items: Vec<crate::peek::GhItem> },
    /// `github_checks`.
    GithubChecks { checks: crate::peek::GhChecks },
    /// `page_peek`.
    Page { page: crate::peek::PageMeta },
    /// `aside`: the fork's answer, its last words (markdown).
    Aside { text: String },
    /// A run's or an object's answer, an open record whose shape runs.rs and object.rs define.
    Record(Value),
}

/// One session as `list`, `screen` and `attach` describe it: a live one, or a past one from the index.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct SessionInfo {
    pub id: u64,
    /// `s<id>`.
    pub object: String,
    /// claude | codex | cursor | shell.
    pub kind: String,
    pub name: Option<String>,
    /// What the person named it ([`Request::Rename`]), shown in place of its `name`; None until it is renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub parent: Option<u64>,
    /// A chat's first user line, a terminal's title.
    pub title: String,
    pub cwd: Option<String>,
    pub argv: Vec<String>,
    pub created_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    pub exit: Option<i64>,
    /// running | exited | killed | lost.
    pub status: Option<String>,
    /// How many clients are attached.
    pub attached: u64,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub busy: bool,
    /// chat | tui.
    pub mode: String,
    /// A chat's model, as its agent said.
    pub model: Option<String>,
    /// A chat's reasoning effort, as its launch chose or its agent said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// It ended before this daemon started (set by `list`).
    #[serde(default)]
    pub past: bool,
}

impl Up {
    /// A request as the message that carries it, encoded.
    #[must_use]
    pub fn req(r: Request) -> Vec<u8> {
        crate::cbor::encode(&Self::Req(r))
    }
}
