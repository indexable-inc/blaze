//! The session core: what a session answers and sends, with no transport in it. Its clock is
//! the driver's (`at_ns` arguments), it owns no socket, thread or timer, and each step hands back
//! a typed decision the driver carries out ([`Step`], [`Emit`], the bytes of a header, an
//! answer's header). An answer's Arrow IPC is [`frame::answer_bytes`]'s: arrow's IPC writer keys
//! its dictionaries in a `HashMap` of `RandomState`, OS randomness a core does not reach.
//!
//! For the same reason the core makes no `serde_json::Map` and parses no `serde_json::Value`:
//! where a crate of the build turns on serde_json's `preserve_order`, a `Map` is an `IndexMap`
//! seeded from OS randomness. A header the core writes is a typed value serialized field by
//! field ([`HelloHeader`], [`LiveHeader`], [`CertHeader`], [`CoreHeader`],
//! [`super::source::ErrorHeader`]), so its bytes are the same in every build; of a request the
//! core reads what it decides on (its `op`, a hello's token), and the driver, which holds the
//! request's bytes, parses them whole for the source.
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

use serde::{
    de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor},
    ser::{Serialize, SerializeMap, SerializeSeq, Serializer},
};

use super::{
    PROTOCOL, frame,
    link::Token,
    source::{Answer, ErrorHeader, Hello, Kind, Layer, OpError, Rule, Table, Tick},
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
    /// Op `name`, which the source answers ([`super::Source::op`]) with the request whole: a
    /// JSON object, which the driver parses from the bytes it gave [`Session::request`].
    Op { name: String },
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
        let (op, request) = match parse(bytes) {
            Ok(it) => it,
            Err(why) => return Step::Refuse(why),
        };
        if op == "hello" {
            return self.hello(token, &request);
        }
        match self.phase {
            Phase::Open if op == "core" => Step::Core,
            Phase::Open => Step::Op { name: op },
            Phase::Hello => Step::Refuse(OpError::new(
                Kind::Unauthenticated,
                "the first request of a session is hello with the link's token",
            )),
            Phase::Closed => Step::Refuse(closed()),
        }
    }

    fn hello(&mut self, token: &Token, request: &Request) -> Step {
        let given = match token_of(request) {
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

/// The request in `bytes`: its op, and what the session reads of it.
fn parse(bytes: &[u8]) -> Result<(String, Request), OpError> {
    let mut request: Request = serde_json::from_slice(bytes)
        .map_err(|why| OpError::new(Kind::BadArgs, format!("the request is not JSON: {why}")))?;
    match request.op.take() {
        Some(Given::Text(op)) => Ok((op, request)),
        _ => Err(OpError::new(Kind::BadArgs, "the request has no string `op`")),
    }
}

/// The token of a `hello`: its one field beside `op`.
fn token_of(request: &Request) -> Result<&str, String> {
    if let Some(extra) = &request.extra {
        return Err(format!("unknown field `{extra}`, expected `token`"));
    }
    match &request.token {
        Some(Given::Text(token)) => Ok(token),
        Some(Given::Other(what)) => Err(format!("`token` is {what}, not a string")),
        None => Err("missing field `token`".to_owned()), // alloc: an error path
    }
}

/// What the session reads of a request: its `op`, a hello's `token`, and the first field that
/// is neither, which a hello may not have. Any JSON but an object reads as a request with no
/// `op`. The request's other fields are the source's, which is given the request whole by the
/// driver: nothing of them is kept here.
#[derive(Debug, Default)]
struct Request {
    op: Option<Given>,
    token: Option<Given>,
    extra: Option<String>,
}

/// What a field the session reads holds: its text, or what else it is, for the error.
#[derive(Debug)]
enum Given {
    Text(String),
    Other(&'static str),
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Request, D::Error> {
        struct Read;

        impl<'de> Visitor<'de> for Read {
            type Value = Request;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a request: a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Request, A::Error> {
                let mut request = Request::default();
                // alloc: once a field of a request, its name
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "op" => request.op = Some(map.next_value()?),
                        "token" => request.token = Some(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                            request.extra.get_or_insert(key);
                        }
                    }
                }
                Ok(request)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Request, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Request::default())
            }

            fn visit_bool<E>(self, _: bool) -> Result<Request, E> {
                Ok(Request::default())
            }

            fn visit_i64<E>(self, _: i64) -> Result<Request, E> {
                Ok(Request::default())
            }

            fn visit_u64<E>(self, _: u64) -> Result<Request, E> {
                Ok(Request::default())
            }

            fn visit_f64<E>(self, _: f64) -> Result<Request, E> {
                Ok(Request::default())
            }

            fn visit_str<E>(self, _: &str) -> Result<Request, E> {
                Ok(Request::default())
            }

            fn visit_unit<E>(self) -> Result<Request, E> {
                Ok(Request::default())
            }
        }

        from.deserialize_any(Read)
    }
}

impl<'de> Deserialize<'de> for Given {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Given, D::Error> {
        struct Read;

        impl<'de> Visitor<'de> for Read {
            type Value = Given;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_str<E>(self, text: &str) -> Result<Given, E> {
                // alloc: once a request, its op's name or its token
                Ok(Given::Text(text.to_owned()))
            }

            fn visit_string<E>(self, text: String) -> Result<Given, E> {
                Ok(Given::Text(text))
            }

            fn visit_bool<E>(self, _: bool) -> Result<Given, E> {
                Ok(Given::Other("a boolean"))
            }

            fn visit_i64<E>(self, _: i64) -> Result<Given, E> {
                Ok(Given::Other("a number"))
            }

            fn visit_u64<E>(self, _: u64) -> Result<Given, E> {
                Ok(Given::Other("a number"))
            }

            fn visit_f64<E>(self, _: f64) -> Result<Given, E> {
                Ok(Given::Other("a number"))
            }

            fn visit_unit<E>(self) -> Result<Given, E> {
                Ok(Given::Other("null"))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Given, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Given::Other("a list"))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Given, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Given::Other("an object"))
            }
        }

        from.deserialize_any(Read)
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
pub fn hello_header<'a>(
    hello: &'a Hello,
    served: &'a Served,
    live_every: Duration,
    core: Option<&'a CoreWasm>,
) -> HelloHeader<'a> {
    HelloHeader { hello, served, live_every, core }
}

/// [`hello_header`]: `op`, `daemon`, `schema`, `clock`, `now_ns`, `certhash`,
/// `cert_not_after_unix_s`, `live_every_ms`, `source: {kind, extensions}`, `protocol`,
/// `layers: [{name, version}]`, and `core` ([`CoreHeader`]) from a server that serves one.
#[derive(Clone, Copy, Debug)]
pub struct HelloHeader<'a> {
    hello: &'a Hello,
    served: &'a Served,
    live_every: Duration,
    core: Option<&'a CoreWasm>,
}

impl Serialize for HelloHeader<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let HelloHeader { hello, served, live_every, core } = *self;
        let mut map = to.serialize_map(Some(11 + usize::from(core.is_some())))?;
        map.serialize_entry("op", "hello")?;
        map.serialize_entry("daemon", &hello.daemon)?;
        map.serialize_entry("schema", &hello.schema)?;
        map.serialize_entry("clock", hello.clock)?;
        map.serialize_entry("now_ns", &hello.now_ns)?;
        map.serialize_entry("certhash", &served.hash)?;
        map.serialize_entry("cert_not_after_unix_s", &served.not_after_s)?;
        map.serialize_entry("live_every_ms", &(live_every.as_millis() as u64))?;
        map.serialize_entry("source", &SourceHeader(hello))?;
        map.serialize_entry("protocol", &PROTOCOL)?;
        map.serialize_entry("layers", &Layers(hello.layers))?;
        if let Some(core) = core {
            map.serialize_entry("core", &CoreHeader { core, op: false })?;
        }
        map.end()
    }
}

/// hello's `source`: `{kind, extensions}`.
struct SourceHeader<'a>(&'a Hello);

impl Serialize for SourceHeader<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let mut map = to.serialize_map(Some(2))?;
        map.serialize_entry("kind", self.0.kind)?;
        map.serialize_entry("extensions", self.0.extensions)?;
        map.end()
    }
}

/// hello's `layers`: `[{name, version}]`.
struct Layers<'a>(&'a [Layer]);

impl Serialize for Layers<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        struct One<'a>(&'a Layer);

        impl Serialize for One<'_> {
            fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
                let mut map = to.serialize_map(Some(2))?;
                map.serialize_entry("name", self.0.name)?;
                map.serialize_entry("version", &self.0.version)?;
                map.end()
            }
        }

        let mut seq = to.serialize_seq(Some(self.0.len()))?;
        for layer in self.0 {
            seq.serialize_element(&One(layer))?;
        }
        seq.end()
    }
}

/// What `hello` and the `core` answer say of a core: its ABI, the hex SHA-256 of its bytes and
/// how many there are; and `op: "core"` after them in the `core` answer's own header.
#[derive(Clone, Copy, Debug)]
pub struct CoreHeader<'a> {
    core: &'a CoreWasm,
    op: bool,
}

impl Serialize for CoreHeader<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let mut map = to.serialize_map(Some(3 + usize::from(self.op)))?;
        map.serialize_entry("abi", &self.core.abi)?;
        map.serialize_entry("sha256", &self.core.sha256)?;
        map.serialize_entry("bytes", &self.core.bytes.len())?;
        if self.op {
            map.serialize_entry("op", "core")?;
        }
        map.end()
    }
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
    // alloc: once a `core` answer, the bytes the driver writes
    let mut out = Vec::with_capacity(core.bytes.len() + 256);
    head(&mut out, &CoreHeader { core, op: true });
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
pub fn live_header<'a>(table: &'a str, rows: usize, tick: &Tick) -> LiveHeader<'a> {
    LiveHeader { table, rows, at_ns: tick.at_ns, skipped: tick.skipped, lost: tick.lost }
}

/// [`live_header`]: `op: "live"`, `table`, `rows`, `at_ns`, `skipped`, `lost`.
#[derive(Clone, Copy, Debug)]
pub struct LiveHeader<'a> {
    table: &'a str,
    rows: usize,
    at_ns: i64,
    skipped: u64,
    lost: u64,
}

impl Serialize for LiveHeader<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let mut map = to.serialize_map(Some(6))?;
        map.serialize_entry("op", "live")?;
        map.serialize_entry("table", self.table)?;
        map.serialize_entry("rows", &self.rows)?;
        map.serialize_entry("at_ns", &self.at_ns)?;
        map.serialize_entry("skipped", &self.skipped)?;
        map.serialize_entry("lost", &self.lost)?;
        map.end()
    }
}

/// The header of a tick that failed: its error, `op: live`.
pub fn live_error_header(error: &OpError) -> ErrorHeader<'_> {
    error.header().with_op("live")
}

/// The header of the live stream's `cert` frame: the certificate was made anew.
pub fn cert_header(served: &Served) -> CertHeader<'_> {
    CertHeader(served)
}

/// [`cert_header`]: `op: "cert"`, `certhash`, `not_after_unix_s`.
#[derive(Clone, Copy, Debug)]
pub struct CertHeader<'a>(&'a Served);

impl Serialize for CertHeader<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let mut map = to.serialize_map(Some(3))?;
        map.serialize_entry("op", "cert")?;
        map.serialize_entry("certhash", &self.0.hash)?;
        map.serialize_entry("not_after_unix_s", &self.0.not_after_s)?;
        map.end()
    }
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

/// [`frame::head`] of one of the core's typed headers: an object of strings, integers and
/// lists and objects of them (an error's detail holds JSON values), which serde_json always
/// writes.
fn head<H: Serialize>(out: &mut Vec<u8>, header: &H) {
    frame::head(out, header).expect("a header of strings and integers always serializes");
}
