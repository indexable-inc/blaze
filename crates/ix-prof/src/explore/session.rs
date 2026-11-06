//! The session core: what a session answers and sends, with no transport in it. Its clock is
//! the driver's (`at_ns` arguments), it owns no socket, thread or timer, and each step hands back
//! a typed decision the driver carries out ([`Step`], [`Emit`], the bytes of a header, an
//! answer's header). An answer's Arrow IPC is [`frame::answer_bytes`]'s: arrow's IPC writer keys
//! its dictionaries in a `HashMap` of `RandomState`, OS randomness a core does not reach.
//!
//! - **Hello first.** A session's first good request is `hello` with the link's token: until
//!   then every other op is answered `unauthenticated`. A wrong token is answered `bad_token`
//!   and the session closed with [`CLOSE_BAD_TOKEN`]. After hello every op but `hello` is the
//!   source's ([`Step::Op`]); a later `hello` with the token is answered again.
//! - **In flight.** A session has at most [`MAX_IN_FLIGHT`] requests in flight, each holding a
//!   [`Slot`] from the stream's opening until its answer is written or dropped; one more is
//!   answered `overloaded`.
//! - **Live.** A tick sends a frame of each table with rows; an [`Rule::Replace`] table (`open`)
//!   that empties is sent once as a frame of no rows, since the page replaces its last set
//!   ([`Feed::emit`]).
//!
//! The wire is explore-wire.md's: a request is one UTF-8 JSON object on its own bidirectional
//! stream, `{"op": ..., ...args}`; a field an op does not take is an error, never ignored.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::time::Duration;

use serde_json::{Map, Value};

use super::{
    PROTOCOL, frame,
    link::Token,
    source::{Answer, Hello, Kind, OpError, Rule, Table, Tick},
    wasm::CoreWasm,
};

/// Requests one session may have in flight.
pub const MAX_IN_FLIGHT: usize = 16;
/// The longest request: SQL a person or agent wrote, never data.
pub const MAX_REQUEST: usize = 1 << 20;
/// How long a request may take to arrive whole.
pub const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// The code a session is closed with after `bad_token`.
pub const CLOSE_BAD_TOKEN: u32 = 1;
/// The code every session is closed with when the explorer stops.
pub const CLOSE_STOPPED: u32 = 0;

/// Where a session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No good hello yet.
    Hello,
    /// Hello answered: the live stream is open and the source answers.
    Open,
    /// Closed after a bad token: nothing more is answered.
    Closed,
}

/// One session's requests.
#[derive(Debug)]
pub struct Session {
    /// The command that prints the server's link (`blaze explore`), which `bad_token` names.
    name: &'static str,
    phase: Phase,
    in_flight: usize,
}

/// A request in flight, from its stream's opening until [`Session::ended`]; not `Clone`, so it
/// ends once.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a slot is given back with `Session::ended`"]
pub struct Slot {
    started_ns: i64,
}

impl Slot {
    /// When its stream opened, on the driver's clock.
    pub fn started_ns(&self) -> i64 {
        self.started_ns
    }

    /// How long since its stream opened, at `at_ns`: the answer's `elapsed_ns`.
    pub fn elapsed_ns(&self, at_ns: i64) -> u64 {
        elapsed_ns(self.started_ns, at_ns)
    }
}

/// The nanoseconds from `started_ns` to `at_ns` on one clock, which never goes back: `at_ns`
/// before `started_ns` is not a later reading of it, and is 0 long.
pub fn elapsed_ns(started_ns: i64, at_ns: i64) -> u64 {
    u64::try_from(at_ns.saturating_sub(started_ns)).unwrap_or(0)
}

/// What the driver does with a request that arrived whole.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// `hello` with the token: answer [`hello_header`]. `first` is the session's first good
    /// hello, after which the driver opens the live stream.
    Hello { first: bool },
    /// Op `name`, which the source answers ([`super::Source::op`]), `args` the request whole.
    Op { name: String, args: Value },
    /// `core`: answer [`core_bytes`], the explorer core the server serves.
    Core,
    /// Answer the error; the session goes on.
    Refuse(OpError),
    /// Answer the error, then close the session with `code`.
    Close { error: OpError, code: u32 },
}

impl Session {
    /// A session of the server whose link `name` prints.
    pub fn new(name: &'static str) -> Session {
        Session { name, phase: Phase::Hello, in_flight: 0 }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// The page opened a request stream at `at_ns`: its slot, or `overloaded` when
    /// [`MAX_IN_FLIGHT`] are in flight.
    pub fn opened(&mut self, at_ns: i64) -> Result<Slot, OpError> {
        if self.in_flight >= MAX_IN_FLIGHT {
            let message =
                format!("the session has {MAX_IN_FLIGHT} requests in flight: wait for one to end");
            return Err(OpError::new(Kind::Overloaded, message).with("in_flight", MAX_IN_FLIGHT));
        }
        self.in_flight += 1;
        Ok(Slot { started_ns: at_ns })
    }

    /// `slot`'s request ended, however it ended (answered, refused, stopped by the page).
    pub fn ended(&mut self, slot: Slot) {
        // Moved in and dropped: a slot ends once, so it is taken by value, never by reference.
        drop(slot);
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// What to do with `bytes`, a request stream's whole request, given the server's `token`.
    pub fn request(&mut self, token: &Token, bytes: &[u8]) -> Step {
        let (op, value) = match parse(bytes) {
            Ok(it) => it,
            Err(why) => return Step::Refuse(why),
        };
        if op == "hello" {
            return self.hello(token, &value);
        }
        match self.phase {
            Phase::Open if op == "core" => Step::Core,
            Phase::Open => Step::Op { name: op, args: value },
            Phase::Hello => Step::Refuse(OpError::new(
                Kind::Unauthenticated,
                "the first request of a session is hello with the link's token",
            )),
            Phase::Closed => Step::Refuse(closed()),
        }
    }

    fn hello(&mut self, token: &Token, value: &Value) -> Step {
        let given = match token_of(value) {
            Ok(it) => it,
            Err(why) => {
                return Step::Refuse(
                    OpError::new(Kind::BadArgs, format!("`hello`: {why}")).with("op", "hello"),
                );
            }
        };
        if !token.matches(given) {
            self.phase = Phase::Closed;
            let message =
                format!("the token is not this server's: run `{}` for its link", self.name);
            return Step::Close {
                error: OpError::new(Kind::BadToken, message),
                code: CLOSE_BAD_TOKEN,
            };
        }
        match self.phase {
            Phase::Hello => {
                self.phase = Phase::Open;
                Step::Hello { first: true }
            }
            Phase::Open => Step::Hello { first: false },
            Phase::Closed => Step::Refuse(closed()),
        }
    }
}

fn closed() -> OpError {
    OpError::new(Kind::Unauthenticated, "the session is closed: its hello had a bad token")
}

/// The request in `bytes`: its op and the whole object.
fn parse(bytes: &[u8]) -> Result<(String, Value), OpError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|why| OpError::new(Kind::BadArgs, format!("the request is not JSON: {why}")))?;
    let op = match value.get("op") {
        // clone: once a request, its op's name
        Some(Value::String(op)) => op.clone(),
        _ => return Err(OpError::new(Kind::BadArgs, "the request has no string `op`")),
    };
    Ok((op, value))
}

/// The token of a `hello`: its one field beside `op`.
fn token_of(value: &Value) -> Result<&str, String> {
    let Value::Object(fields) = value else {
        return Err("the request is not a JSON object".to_owned()); // alloc: an error path
    };
    if let Some(extra) = fields.keys().find(|it| !matches!(it.as_str(), "op" | "token")) {
        return Err(format!("unknown field `{extra}`, expected `token`"));
    }
    match fields.get("token") {
        Some(Value::String(token)) => Ok(token),
        Some(other) => Err(format!("`token` is {other}, not a string")),
        None => Err("missing field `token`".to_owned()), // alloc: an error path
    }
}

/// The error of a request over [`MAX_REQUEST`] bytes.
pub fn too_long() -> OpError {
    OpError::new(Kind::BadArgs, format!("the request is over {MAX_REQUEST} bytes"))
}

/// The error of a request not read whole: the stream broke.
pub fn unread(why: impl std::fmt::Display) -> OpError {
    OpError::new(Kind::BadArgs, format!("the request was not read whole: {why}"))
}

/// The error of a request not whole within [`REQUEST_WAIT`].
pub fn late() -> OpError {
    OpError::new(
        Kind::BadArgs,
        format!(
            "no whole request in {} s: write one JSON object, then close the stream's side",
            REQUEST_WAIT.as_secs()
        ),
    )
}

/// The answer to op `name`, which the source does not have.
pub fn unknown_op(name: &str, hello: &Hello) -> OpError {
    // alloc: an error path
    let layers: Vec<String> =
        hello.layers.iter().map(|it| format!("{} {}", it.name, it.version)).collect();
    let message = format!(
        "there is no op `{name}`: this server ({}) answers `hello` and the ops of its layers ({})",
        hello.kind,
        layers.join(", ")
    );
    OpError::new(Kind::UnknownOp, message).with("op", name)
}

/// The certificate served now, as sessions are told it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    /// [`super::link::certhash`] of its DER.
    pub hash: String,
    /// When it expires, in Unix seconds.
    pub not_after_s: u64,
}

/// The header of `hello`: the source's fields, the certificate served, the live stream's
/// period, and the core's `protocol` beside the source's `layers`.
pub fn hello_header(
    hello: &Hello,
    served: &Served,
    live_every: Duration,
    core: Option<&CoreWasm>,
) -> Map<String, Value> {
    let mut source = Map::new();
    // alloc: once a session
    source.insert("kind".to_owned(), hello.kind.into());
    // alloc: once a session
    source.insert("extensions".to_owned(), hello.extensions.iter().copied().collect());
    let layers: Value = hello
        .layers
        .iter()
        .map(|layer| {
            let mut it = Map::new();
            // alloc: once a session
            it.insert("name".to_owned(), layer.name.into());
            // alloc: once a session
            it.insert("version".to_owned(), layer.version.into());
            Value::Object(it)
        })
        .collect();
    let header = [
        ("op", Value::from("hello")),
        ("daemon", hello.daemon.as_str().into()),
        ("schema", hello.schema.into()),
        ("clock", hello.clock.into()),
        ("now_ns", hello.now_ns.into()),
        ("certhash", served.hash.as_str().into()),
        ("cert_not_after_unix_s", served.not_after_s.into()),
        ("live_every_ms", (live_every.as_millis() as u64).into()),
        ("source", Value::Object(source)),
        ("protocol", PROTOCOL.into()),
        ("layers", layers),
    ];
    // alloc: once a session
    let mut header: Map<String, Value> =
        header.into_iter().map(|(key, value)| (key.to_owned(), value)).collect();
    if let Some(core) = core {
        // alloc: once a session
        header.insert("core".to_owned(), core_header(core).into());
    }
    header
}

/// What `hello` and the `core` answer say of a core: its ABI, the hex SHA-256 of its bytes and
/// how many there are.
fn core_header(core: &CoreWasm) -> Map<String, Value> {
    let fields = [
        ("abi", Value::from(core.abi)),
        ("sha256", core.sha256.as_str().into()),
        ("bytes", core.bytes.len().into()),
    ];
    // alloc: once a session, or a `core` answer
    fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

/// The `core` answer's bytes: a header naming the core (`op`, `abi`, `sha256`, `bytes`), then
/// the module's raw bytes (not Arrow), starting [`frame::ALIGN`]-aligned; or, for a server with
/// no core, the error `no_core` saying why (where the server looked and how to make one).
pub fn core_bytes(core: &Result<CoreWasm, String>) -> Vec<u8> {
    let core = match core {
        Ok(core) => core,
        // clone: an error path
        Err(why) => return error_bytes(&OpError::new(Kind::NoCore, why.clone()).with("op", "core")),
    };
    let mut header = core_header(core);
    // alloc: once a `core` answer
    header.insert("op".to_owned(), "core".into());
    // alloc: once a `core` answer, the bytes the driver writes
    let mut out = Vec::with_capacity(core.bytes.len() + 256);
    head(&mut out, &header);
    out.extend_from_slice(&core.bytes);
    out
}

/// What a tick sends of one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emit {
    Nothing,
    /// A frame with its rows and IPC.
    Rows,
    /// A frame of `rows: 0` and no IPC: a [`Rule::Replace`] set the page shows emptied. The page
    /// replaces the last set, so it needs the empty one once; an [`Rule::Append`] or
    /// [`Rule::Upsert`] table never needs it.
    Cleared,
}

/// One session's live stream: which [`Rule::Replace`] tables the page shows a non-empty set of.
#[derive(Debug, Default)]
pub struct Feed {
    shown: Vec<&'static str>,
}

impl Feed {
    /// What this tick sends of `table`, of `rule`, which has `rows`; the page is taken to show
    /// what it is sent.
    pub fn emit(&mut self, table: &'static str, rule: Rule, rows: usize) -> Emit {
        let at = self.shown.iter().position(|it| *it == table);
        let emit = match (rows, rule) {
            (0, Rule::Replace) if at.is_some() => Emit::Cleared,
            (0, _) => Emit::Nothing,
            _ => Emit::Rows,
        };
        if rule == Rule::Replace {
            match (at, rows > 0) {
                (Some(at), false) => {
                    self.shown.swap_remove(at);
                }
                // alloc: once a table a session
                (None, true) => self.shown.push(table),
                _ => {}
            }
        }
        emit
    }
}

/// The header of `table`'s frame of `tick`, which has `rows`.
pub fn live_header(table: &str, rows: usize, tick: &Tick) -> Map<String, Value> {
    let header = [
        ("op", Value::from("live")),
        ("table", table.into()),
        ("rows", rows.into()),
        ("at_ns", tick.at_ns.into()),
        ("skipped", tick.skipped.into()),
        ("lost", tick.lost.into()),
    ];
    // alloc: a live frame's header, a few a tick
    header.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

/// The header of a tick that failed: its error, `op: live`.
pub fn live_error_header(error: &OpError) -> Map<String, Value> {
    let mut header = error.header();
    // alloc: an error path
    header.insert("op".to_owned(), "live".into());
    header
}

/// The header of the live stream's `cert` frame: the certificate was made anew.
pub fn cert_header(served: &Served) -> Map<String, Value> {
    let mut header = Map::new();
    // alloc: every 12 days
    header.insert("op".to_owned(), "cert".into());
    // alloc: every 12 days
    header.insert("certhash".to_owned(), served.hash.as_str().into());
    // alloc: every 12 days
    header.insert("not_after_unix_s".to_owned(), served.not_after_s.into());
    header
}

/// What the core adds to an answer's header: `rows` when it has a table, and `elapsed_ns`.
/// The bytes are [`frame::answer_bytes`]'s, which calls this.
pub fn answer_header(answer: &mut Answer, elapsed_ns: u64) {
    if let Some(rows) = answer.table.as_ref().map(Table::rows) {
        // alloc: once an answer
        answer.header.insert("rows".to_owned(), rows.into());
    }
    // alloc: once an answer
    answer.header.insert("elapsed_ns".to_owned(), elapsed_ns.into());
}

/// An error as the wire carries it: its header alone.
pub fn error_bytes(error: &OpError) -> Vec<u8> {
    // alloc: an error path
    let mut out = Vec::new();
    head(&mut out, &error.header());
    out
}

/// The `hello` answer's bytes.
pub fn hello_bytes(
    hello: &Hello,
    served: &Served,
    live_every: Duration,
    core: Option<&CoreWasm>,
) -> Vec<u8> {
    // alloc: once a session
    let mut out = Vec::new();
    head(&mut out, &hello_header(hello, served, live_every, core));
    out
}

/// [`frame::head`] of a header that is a JSON map of strings to JSON values, which serde_json
/// always writes.
fn head(out: &mut Vec<u8>, header: &Map<String, Value>) {
    frame::head(out, header).expect("a JSON map always serializes");
}
