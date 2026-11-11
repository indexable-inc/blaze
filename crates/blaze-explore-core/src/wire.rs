//! The daemon's framing, decoded exactly as blaze-explore writes it (its `frame` and `server`
//! modules; docs/v2/plan/design/explore.md, "Frames").
//!
//! - An **answer**, on a request's stream: `u32 LE` header length, the header's JSON padded
//!   with spaces so the IPC after it starts 8-aligned, then one Arrow IPC stream or nothing.
//!   A refused request's header is `{"error": {"kind", "message", ...detail}}` and has no IPC.
//! - **hello** is the answer to the first request: `daemon`, `schema`, `clock`, `now_ns`,
//!   `certhash`, `cert_not_after_unix_s`, `live_every_ms` and the layers it serves, in
//!   `layers: [{name, version}]` (the generic protocol, with `protocol`) or
//!   `source.extensions` (blaze-explore today).
//! - The **live stream**, one unidirectional stream: frames back to back, each a `u32 LE`
//!   length and then an answer laid out as above. A frame's header is `op: "live"` with
//!   `table`, `rows`, `at_ns`, `skipped`, `lost` (its IPC the next messages of that table's
//!   stream, [`crate::live::Decoder`], none when `rows` is 0), `op: "cert"` when the
//!   certificate is made anew, or an error header with `op: "live"` beside it.
//!
//! Integers stay exact: serde_json reads a JSON integer into a `u64` or `i64` whole, and a
//! field that holds anything else is refused, naming the message and the field.
//!
//! A header is read into this module's own [`Json`] and [`Object`], by serde_json's parser,
//! never into `serde_json::Value`: where a crate of the build turns on serde_json's
//! `preserve_order`, its `Map` is an `IndexMap` seeded from OS randomness, which a sans-IO
//! core does not reach (and which the page's wasm has none of). An [`Object`] is its fields in
//! the order the daemon wrote them.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use serde::{
    de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor},
    ser::{Serialize, SerializeMap, Serializer},
};

use crate::{Error, field};

/// A JSON value of a header.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// An integer that is not negative, whole.
    U64(u64),
    /// A negative integer, whole.
    I64(i64),
    /// A number that is no integer of 64 bits.
    F64(f64),
    String(String),
    Array(Vec<Json>),
    Object(Object),
}

impl Json {
    /// The integer it holds, when that is one a `u64` holds.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Json::U64(it) => Some(it),
            Json::I64(it) => u64::try_from(it).ok(),
            _ => None,
        }
    }

    /// The integer it holds, when that is one an `i64` holds.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Json::I64(it) => Some(it),
            Json::U64(it) => i64::try_from(it).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(it) => Some(it),
            _ => None,
        }
    }

    /// The field `name` of an object; `None` for any other value.
    pub fn get(&self, name: &str) -> Option<&Json> {
        match self {
            Json::Object(it) => it.get(name),
            _ => None,
        }
    }
}

/// As JSON text, for an error that shows what a field holds.
impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // alloc: an error's text
        let text = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        f.write_str(&text)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => to.serialize_unit(),
            Json::Bool(it) => to.serialize_bool(*it),
            Json::U64(it) => to.serialize_u64(*it),
            Json::I64(it) => to.serialize_i64(*it),
            Json::F64(it) => to.serialize_f64(*it),
            Json::String(it) => to.serialize_str(it),
            Json::Array(it) => to.collect_seq(it),
            Json::Object(it) => it.serialize(to),
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Json, D::Error> {
        struct Read;

        impl<'de> Visitor<'de> for Read {
            type Value = Json;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }

            fn visit_bool<E>(self, it: bool) -> Result<Json, E> {
                Ok(Json::Bool(it))
            }

            fn visit_u64<E>(self, it: u64) -> Result<Json, E> {
                Ok(Json::U64(it))
            }

            fn visit_i64<E>(self, it: i64) -> Result<Json, E> {
                Ok(match u64::try_from(it) {
                    Ok(it) => Json::U64(it),
                    Err(_) => Json::I64(it),
                })
            }

            fn visit_f64<E>(self, it: f64) -> Result<Json, E> {
                Ok(Json::F64(it))
            }

            fn visit_str<E>(self, it: &str) -> Result<Json, E> {
                // alloc: once a string of a header
                Ok(Json::String(it.to_owned()))
            }

            fn visit_string<E>(self, it: String) -> Result<Json, E> {
                Ok(Json::String(it))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                // alloc: once a list of a header
                let mut list = Vec::new();
                while let Some(it) = seq.next_element()? {
                    list.push(it);
                }
                Ok(Json::Array(list))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut object = Object::default();
                // alloc: once a field of a header, its name
                while let Some((name, value)) = map.next_entry::<String, Json>()? {
                    object.insert(name, value);
                }
                Ok(Json::Object(object))
            }
        }

        from.deserialize_any(Read)
    }
}

/// A JSON object of a header: its fields in the order written. A name written twice holds
/// its last value, in its first place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object(Vec<(String, Json)>);

impl Object {
    pub fn get(&self, name: &str) -> Option<&Json> {
        self.0.iter().find(|(it, _)| it == name).map(|(_, value)| value)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The fields' names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    /// The fields, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Json)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Sets `name`, in the place it has when it is there already.
    pub fn insert(&mut self, name: String, value: Json) {
        match self.0.iter_mut().find(|(it, _)| *it == name) {
            Some((_, held)) => *held = value,
            None => self.0.push((name, value)),
        }
    }

    /// Takes `name` out, the fields after it keeping their order.
    pub fn remove(&mut self, name: &str) -> Option<Json> {
        let at = self.0.iter().position(|(it, _)| it == name)?;
        Some(self.0.remove(at).1)
    }

    /// Moves every field of `other` into this object, after its own: one it has already takes
    /// `other`'s value. `other` is left empty.
    pub fn append(&mut self, other: &mut Object) {
        for (name, value) in other.0.drain(..) {
            self.insert(name, value);
        }
    }
}

impl Serialize for Object {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let mut map = to.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// The bytes of a length: an answer's header length, a live frame's length.
pub const HEAD: usize = 4;

/// The longest header taken: the daemon's are a few hundred bytes; a view's `sql` is under
/// blaze-explore's `MAX_REQUEST`.
pub const MAX_HEADER: usize = 1 << 20;

/// The longest live frame taken: a frame is one tick's rows of one table, far under this; a
/// longer length is a stream out of step, not a frame.
pub const MAX_FRAME: usize = 256 << 20;

/// The versions of blaze-profile's schema (`schema::VERSION`, hello's `schema`) whose live
/// tables this core reads. The core is built from the daemon's tree (explore-core.md decision
/// 3), and a test holds this to blaze-profile's own.
pub const SCHEMAS: [u32; 1] = [5];

/// An answer's header and the IPC after it.
#[derive(Debug)]
pub struct Head<'a> {
    pub header: Object,
    pub ipc: &'a [u8],
}

/// A request the daemon refused, as its error header says.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub kind: String,
    pub message: String,
    /// The rest of the error object: `op`, `table`, `in_flight`, `query`.
    pub detail: Object,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// What an answer is: a header with its IPC, or a refusal.
#[derive(Debug)]
pub enum Answered<'a> {
    Head(Head<'a>),
    Refused(Refusal),
}

/// The header and IPC of one whole answer (or a live frame's body), `message` naming it in
/// errors.
pub fn answered<'a>(bytes: &'a [u8], message: &str) -> Result<Answered<'a>, Error> {
    let Some(len) = bytes.first_chunk::<HEAD>() else {
        return Err(Error::new(
            message,
            "the header length",
            // alloc: an error's text
            format!("{} bytes, short of the {HEAD} a header length takes", bytes.len()),
        ));
    };
    let len = u32::from_le_bytes(*len) as usize;
    if len > MAX_HEADER {
        return Err(Error::new(
            message,
            "the header length",
            // alloc: an error's text
            format!("{len} bytes, over the {MAX_HEADER} a header may take"),
        ));
    }
    let Some(text) = bytes.get(HEAD..HEAD + len) else {
        return Err(Error::new(
            message,
            "the header",
            // alloc: an error's text
            format!("{len} bytes run past the {}-byte message", bytes.len()),
        ));
    };
    let ipc = &bytes[HEAD + len..];
    let header: Json = serde_json::from_slice(text).map_err(|why| {
        // alloc: an error's text
        Error::new(message, "the header", format!("not JSON ({why}): {}", shown(text)))
    })?;
    let Json::Object(mut header) = header else {
        // alloc: an error's text
        return Err(Error::new(message, "the header", format!("not an object: {}", shown(text))));
    };
    let Some(error) = header.remove("error") else {
        return Ok(Answered::Head(Head { header, ipc }));
    };
    let Json::Object(mut error) = error else {
        // alloc: an error's text
        return Err(Error::new(message, field("error"), format!("holds {error}, not an object")));
    };
    let mut text = |key: &str| match error.remove(key) {
        Some(Json::String(it)) => Ok(it),
        other => Err(Error::new(
            message,
            // alloc: an error's text
            format!("field `error.{key}`"),
            // alloc: an error's text
            format!("holds {}, not a string", other.unwrap_or(Json::Null)),
        )),
    };
    let (kind, said) = (text("kind")?, text("message")?);
    if !ipc.is_empty() {
        return Err(Error::new(
            message,
            "the IPC",
            // alloc: an error's text
            format!("an error header ({kind}) is followed by {} bytes", ipc.len()),
        ));
    }
    // The header's own fields (`op: "live"` beside a live frame's error) join the detail.
    error.append(&mut header);
    Ok(Answered::Refused(Refusal { kind, message: said, detail: error }))
}

/// At most 200 bytes of `text`, for an error.
fn shown(text: &[u8]) -> String {
    let end = text.len().min(200);
    // alloc: an error's text
    String::from_utf8_lossy(&text[..end]).into_owned()
}

/// A header's fields, read typed, errors naming `message` and the field.
pub(crate) struct Fields<'h> {
    pub(crate) header: &'h Object,
    pub(crate) message: &'h str,
}

impl<'h> Fields<'h> {
    fn wrong(&self, name: &str, value: &Json, want: &str) -> Error {
        // alloc: an error's text
        Error::new(self.message, field(name), format!("holds {value}, not {want}"))
    }

    pub(crate) fn get(&self, name: &str) -> Result<&'h Json, Error> {
        self.header.get(name).ok_or_else(|| {
            // alloc: an error's text, once
            let keys: Vec<&str> = self.header.keys().collect();
            // alloc: an error's text
            Error::new(self.message, field(name), format!("missing; the header has {}", keys.join(", ")))
        })
    }

    pub(crate) fn u64(&self, name: &str) -> Result<u64, Error> {
        let value = self.get(name)?;
        value.as_u64().ok_or_else(|| self.wrong(name, value, "an unsigned 64-bit integer"))
    }

    pub(crate) fn i64(&self, name: &str) -> Result<i64, Error> {
        let value = self.get(name)?;
        value.as_i64().ok_or_else(|| self.wrong(name, value, "a signed 64-bit integer"))
    }

    pub(crate) fn u32(&self, name: &str) -> Result<u32, Error> {
        let value = self.get(name)?;
        value
            .as_u64()
            .and_then(|it| u32::try_from(it).ok())
            .ok_or_else(|| self.wrong(name, value, "an unsigned 32-bit integer"))
    }

    pub(crate) fn string(&self, name: &str) -> Result<&'h str, Error> {
        let value = self.get(name)?;
        value.as_str().ok_or_else(|| self.wrong(name, value, "a string"))
    }

    /// A field that may be absent; present, it must be an unsigned integer.
    pub(crate) fn opt_u64(&self, name: &str) -> Result<Option<u64>, Error> {
        match self.header.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| self.wrong(name, value, "an unsigned 64-bit integer")),
        }
    }
}

/// The daemon's `hello`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The daemon's version.
    pub daemon: String,
    /// blaze-profile's `schema::VERSION`, one of [`SCHEMAS`].
    pub schema: u32,
    /// The clock every `_ns` counts on (`darwin CLOCK_UPTIME_RAW`, `linux CLOCK_MONOTONIC`).
    pub clock: String,
    /// That clock when the daemon answered.
    pub now_ns: u64,
    pub certhash: String,
    pub cert_not_after_unix_s: u64,
    pub live_every_ms: u64,
    /// The layers above the core tables (`blaze`: builds, SQL, views, the build's workers).
    pub layers: Vec<String>,
    /// The core protocol's version, from a source that says one.
    pub protocol: Option<u32>,
}

/// The hello answer in `bytes`, one whole answer.
pub fn hello(bytes: &[u8]) -> Result<Hello, Error> {
    const MESSAGE: &str = "hello";
    let head = match answered(bytes, MESSAGE)? {
        Answered::Head(it) => it,
        Answered::Refused(refusal) => {
            // alloc: an error's text
            return Err(Error::new(MESSAGE, "", format!("the daemon refused it: {refusal}")));
        }
    };
    let fields = Fields { header: &head.header, message: MESSAGE };
    let op = fields.string("op")?;
    if op != "hello" {
        // alloc: an error's text
        return Err(Error::new(MESSAGE, field("op"), format!("is `{op}`, not `hello`")));
    }
    if !head.ipc.is_empty() {
        return Err(Error::new(
            MESSAGE,
            "the IPC",
            // alloc: an error's text
            format!("hello has no table, but {} bytes follow its header", head.ipc.len()),
        ));
    }
    let schema = fields.u32("schema")?;
    if !SCHEMAS.contains(&schema) {
        return Err(Error::new(
            MESSAGE,
            field("schema"),
            // alloc: an error's text
            format!(
                "is {schema}, and this explorer core reads {SCHEMAS:?}: the core and the daemon \
                 are from different builds; serve the core built with this daemon"
            ),
        ));
    }
    let hello = Hello {
        // alloc: once a session
        daemon: fields.string("daemon")?.to_owned(),
        schema,
        // alloc: once a session
        clock: fields.string("clock")?.to_owned(),
        now_ns: fields.u64("now_ns")?,
        // alloc: once a session
        certhash: fields.string("certhash")?.to_owned(),
        cert_not_after_unix_s: fields.u64("cert_not_after_unix_s")?,
        live_every_ms: fields.u64("live_every_ms")?,
        layers: layers(&fields)?,
        protocol: match fields.opt_u64("protocol")? {
            None => None,
            Some(it) => Some(u32::try_from(it).map_err(|_| {
                // alloc: an error's text
                Error::new(MESSAGE, field("protocol"), format!("holds {it}, past a u32"))
            })?),
        },
    };
    Ok(hello)
}

/// hello's layers: `layers: [{name, version}]`, or else `source.extensions: [name]`; one of
/// them must be there, as every daemon that serves a core sends `source`.
fn layers(fields: &Fields<'_>) -> Result<Vec<String>, Error> {
    let message = fields.message;
    let names = |list: &Vec<Json>, what: &str, name_of: fn(&Json) -> Option<&str>| {
        list.iter()
            .map(|it| {
                // alloc: once a layer a session
                name_of(it).map(str::to_owned).ok_or_else(|| {
                    // alloc: an error's text
                    Error::new(message, field(what), format!("holds an entry {it}, not {}", what))
                })
            })
            // alloc: once a session
            .collect::<Result<Vec<String>, Error>>()
    };
    if let Some(layers) = fields.header.get("layers") {
        let Json::Array(list) = layers else {
            // alloc: an error's text
            return Err(Error::new(message, field("layers"), format!("holds {layers}, not a list")));
        };
        return names(list, "layers", |it| it.get("name").and_then(Json::as_str));
    }
    let Some(source) = fields.header.get("source") else {
        return Err(Error::new(
            message,
            "fields `layers` and `source`",
            "both missing: hello names its layers in one of them",
        ));
    };
    match source.get("extensions") {
        Some(Json::Array(list)) => names(list, "source.extensions", Json::as_str),
        other => Err(Error::new(
            message,
            field("source.extensions"),
            // alloc: an error's text
            format!("holds {}, not a list of names", other.unwrap_or(&Json::Null)),
        )),
    }
}

/// A live frame's header, its table's IPC after it.
#[derive(Debug, PartialEq, Eq)]
pub struct LiveHead<'a> {
    pub table: String,
    pub rows: u64,
    /// When the daemon read what the frame carries, on hello's clock.
    pub at_ns: i64,
    /// Rows written over before this session read them, since its last frame.
    pub skipped: u64,
    /// Spans the daemon did not record since its process began.
    pub lost: u64,
    /// The next messages of the table's stream; empty exactly when `rows` is 0.
    pub ipc: &'a [u8],
}

/// One live frame.
#[derive(Debug, PartialEq)]
pub enum Live<'a> {
    Rows(LiveHead<'a>),
    /// The certificate was made anew.
    Cert { certhash: String, not_after_unix_s: u64 },
    /// The daemon could not encode this tick's frame; the next tick tries again.
    Refused(Refusal),
}

/// The live frame in `frame`: the bytes after its `u32` length, an answer's layout.
pub fn live(frame: &[u8]) -> Result<Live<'_>, Error> {
    let head = match answered(frame, "live frame")? {
        Answered::Head(it) => it,
        Answered::Refused(refusal) => return Ok(Live::Refused(refusal)),
    };
    let fields = Fields { header: &head.header, message: "live frame" };
    match fields.string("op")? {
        "live" => {}
        "cert" => {
            let fields = Fields { header: &head.header, message: "live cert frame" };
            return Ok(Live::Cert {
                // alloc: every 12 days
                certhash: fields.string("certhash")?.to_owned(),
                not_after_unix_s: fields.u64("not_after_unix_s")?,
            });
        }
        op => {
            return Err(Error::new(
                "live frame",
                field("op"),
                // alloc: an error's text
                format!("is `{op}`; the live stream sends `live` and `cert`"),
            ));
        }
    }
    let table = fields.string("table")?;
    if table.is_empty() {
        return Err(Error::new("live frame", field("table"), "is empty, not a table's name"));
    }
    // alloc: once a frame, the message its errors name
    let message = format!("live frame for table `{table}`");
    let fields = Fields { header: &head.header, message: &message };
    let rows = fields.u64("rows")?;
    if (rows == 0) != head.ipc.is_empty() {
        return Err(Error::new(
            // clone: an error's text
            message.clone(),
            field("rows"),
            // alloc: an error's text
            format!(
                "is {rows} with {} bytes of IPC: a frame has IPC exactly when it has rows",
                head.ipc.len()
            ),
        ));
    }
    Ok(Live::Rows(LiveHead {
        // alloc: once a frame
        table: table.to_owned(),
        rows,
        at_ns: fields.i64("at_ns")?,
        skipped: fields.u64("skipped")?,
        lost: fields.u64("lost")?,
        ipc: head.ipc,
    }))
}

/// An answer to a request other than hello.
#[derive(Debug)]
pub struct Answer<'a> {
    pub op: String,
    /// The table's rows, for an op that answers with one.
    pub rows: Option<u64>,
    pub elapsed_ns: Option<u64>,
    pub header: Object,
    pub ipc: &'a [u8],
}

/// The answer in `bytes`, one whole answer: its header and IPC, or the daemon's refusal.
pub fn answer(bytes: &[u8]) -> Result<Result<Answer<'_>, Refusal>, Error> {
    let head = match answered(bytes, "answer")? {
        Answered::Head(it) => it,
        Answered::Refused(refusal) => return Ok(Err(refusal)),
    };
    let fields = Fields { header: &head.header, message: "answer" };
    // alloc: once an answer
    let op = fields.string("op")?.to_owned();
    // alloc: once an answer, the message its errors name
    let message = format!("answer to `{op}`");
    let fields = Fields { header: &head.header, message: &message };
    let rows = fields.opt_u64("rows")?;
    let elapsed_ns = fields.opt_u64("elapsed_ns")?;
    if rows.is_none() && !head.ipc.is_empty() {
        return Err(Error::new(
            message,
            field("rows"),
            // alloc: an error's text
            format!("missing, with {} bytes of IPC after the header", head.ipc.len()),
        ));
    }
    Ok(Ok(Answer { op, rows, elapsed_ns, header: head.header, ipc: head.ipc }))
}

/// The live stream cut into frames: bytes go in as they arrive, split anywhere, and each whole
/// frame comes out. A length past [`MAX_FRAME`] puts the stream out of step for good: every
/// later call says so.
#[derive(Debug, Default)]
pub struct Frames {
    buf: Vec<u8>,
    /// Where the next frame's length starts in `buf`.
    at: usize,
    broken: Option<Error>,
}

impl Frames {
    pub fn new() -> Frames {
        Frames::default()
    }

    /// Bytes read from the live stream.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        if self.at > 0 && self.at * 2 >= self.buf.len() {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        // alloc: the stream's bytes until a frame is whole, at most a frame and a chunk
        self.buf.extend_from_slice(chunk);
    }

    /// The next whole frame's body (after its length) into `into`, cleared first; `false`
    /// when no frame is whole yet.
    pub fn next_into(&mut self, into: &mut Vec<u8>) -> Result<bool, Error> {
        if let Some(why) = &self.broken {
            // clone: the error, each call after the stream broke
            return Err(why.clone());
        }
        into.clear();
        let rest = &self.buf[self.at..];
        let Some(len) = rest.first_chunk::<HEAD>() else { return Ok(false) };
        let len = u32::from_le_bytes(*len) as usize;
        if len > MAX_FRAME {
            let why = Error::new(
                "the live stream",
                "a frame's length",
                // alloc: an error's text
                format!(
                    "{len} bytes at byte {} of what is unread, over the {MAX_FRAME} a frame may take: the stream is out of step",
                    self.at
                ),
            );
            // clone: the error kept for every later call
            self.broken = Some(why.clone());
            return Err(why);
        }
        let Some(body) = rest.get(HEAD..HEAD + len) else { return Ok(false) };
        // alloc: a frame, into the caller's reused buffer
        into.extend_from_slice(body);
        self.at += HEAD + len;
        Ok(true)
    }

    /// Bytes held that are not yet a whole frame.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.at
    }

    /// Whether the stream ended between frames: the daemon's last frame was whole.
    pub fn end(&self) -> Result<(), Error> {
        match self.pending() {
            0 => Ok(()),
            n => Err(Error::new(
                "the live stream",
                "its end",
                // alloc: an error's text
                format!("it ended {n} bytes into a frame"),
            )),
        }
    }
}
