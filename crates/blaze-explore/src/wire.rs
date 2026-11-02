//! What the page asks and how an answer that failed says why (explore-wire.md, "Ops" and
//! "Errors"). A request is one UTF-8 JSON object on its own bidirectional stream, `{"op": ...,
//! ...args}`; the stream is its id. A field the op does not take is an error, never ignored.

use serde_json::{Map, Value};

use crate::tiles::Lane;

/// Every op, as the page names it.
pub const OPS: [&str; 8] = ["hello", "builds", "sql", "views", "view", "spans", "tiles", "world"];

/// A request, its arguments typed.
#[derive(Debug, PartialEq, serde_derive::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The first request of a session: the token from the link.
    Hello { token: String },
    /// The `builds` table.
    Builds {},
    /// Any query over the history, as DuckDB runs it.
    Sql { sql: String },
    /// The named views: name, title, SQL and the JSON schema of their arguments.
    Views {},
    /// A named view run with `args`; the header's `sql` is what ran.
    View {
        name: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    /// The `spans` rows of build `build` overlapping `[t0_ns, t1_ns)`.
    Spans { build: u64, t0_ns: i64, t1_ns: i64 },
    /// The `tiles` of build `build` over `[t0_ns, t1_ns)`, `bucket_ns` a bucket: a lane per
    /// thread, or with `"lane": "job"` the `job_tiles`, a lane per fiber job.
    Tiles {
        build: u64,
        t0_ns: i64,
        t1_ns: i64,
        bucket_ns: i64,
        #[serde(default)]
        lane: Lane,
    },
    /// Flecs's REST over the build's world: `path` as Flecs takes it, `/entity/...`, `/query?...`.
    World { path: String },
}

/// What went wrong, as the page tells it apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    UnknownOp,
    BadArgs,
    /// The hello's token is not this server's: the session is closed (code 1, `bad_token`).
    BadToken,
    /// A request other than hello before a good hello.
    Unauthenticated,
    /// DuckDB refused the query or could not run; `detail` has its message and the query.
    Sql,
    NoSuchBuild,
    NoSuchView,
    /// The daemon keeps no build world to answer `world` from; `message` names what is missing.
    NoWorld,
    /// Flecs answered the `world` request with an error; `detail.code` is its HTTP status.
    World,
    /// The session has as many requests in flight as it may.
    Overloaded,
    /// The answer could not be encoded as Arrow IPC.
    Encode,
    /// The page stopped the request: DuckDB was killed before it answered.
    Cancelled,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::UnknownOp => "unknown_op",
            Kind::BadArgs => "bad_args",
            Kind::BadToken => "bad_token",
            Kind::Unauthenticated => "unauthenticated",
            Kind::Sql => "sql",
            Kind::NoSuchBuild => "no_such_build",
            Kind::NoSuchView => "no_such_view",
            Kind::NoWorld => "no_world",
            Kind::World => "world",
            Kind::Overloaded => "overloaded",
            Kind::Encode => "encode",
            Kind::Cancelled => "cancelled",
        }
    }
}

/// An answer that failed: its kind, a sentence for a person, and typed detail for the page.
#[derive(Debug, PartialEq)]
pub struct WireError {
    pub kind: Kind,
    pub message: String,
    pub detail: Map<String, Value>,
}

impl WireError {
    pub fn new(kind: Kind, message: impl Into<String>) -> WireError {
        WireError { kind, message: message.into(), detail: Map::new() }
    }

    /// Adds `key` to the detail.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> WireError {
        // alloc: an error path
        self.detail.insert(key.to_owned(), value.into());
        self
    }

    /// The header an error is sent as: `{"error": {"kind", "message", ...detail}}`.
    pub fn header(&self) -> Map<String, Value> {
        let mut error = Map::new();
        // alloc: an error path
        error.insert("kind".to_owned(), self.kind.name().into());
        // alloc: an error path
        error.insert("message".to_owned(), self.message.as_str().into());
        for (key, value) in &self.detail {
            // clone: an error path
            error.insert(key.clone(), value.clone());
        }
        let mut header = Map::new();
        // alloc: an error path
        header.insert("error".to_owned(), Value::Object(error));
        header
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.message)
    }
}

impl std::error::Error for WireError {}

/// The request in `bytes`: an op not in [`OPS`] is `unknown_op`; anything else wrong with it,
/// `bad_args`, naming the op and what is wrong.
pub fn parse(bytes: &[u8]) -> Result<Request, WireError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|why| WireError::new(Kind::BadArgs, format!("the request is not JSON: {why}")))?;
    let op = match value.get("op") {
        // clone: once a request, its op's name for the errors below
        Some(Value::String(op)) => op.clone(),
        _ => return Err(WireError::new(Kind::BadArgs, "the request has no string `op`")),
    };
    if !OPS.contains(&op.as_str()) {
        let message = format!("there is no op `{op}`; the ops are {}", OPS.join(", "));
        return Err(WireError::new(Kind::UnknownOp, message).with("op", op));
    }
    serde_json::from_value(value)
        .map_err(|why| WireError::new(Kind::BadArgs, format!("`{op}`: {why}")).with("op", op))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_op_parses_with_its_arguments() {
        assert_eq!(
            parse(br#"{"op":"hello","token":"ab"}"#),
            Ok(Request::Hello { token: "ab".to_owned() })
        );
        assert_eq!(parse(br#"{"op":"builds"}"#), Ok(Request::Builds {}));
        assert_eq!(parse(br#"{"op":"views"}"#), Ok(Request::Views {}));
        assert_eq!(
            parse(br#"{"op":"sql","sql":"select 1"}"#),
            Ok(Request::Sql { sql: "select 1".to_owned() })
        );
        let Ok(Request::View { name, args }) = parse(br#"{"op":"view","name":"slowest","args":{"n":5}}"#)
        else {
            panic!("a view request parses");
        };
        assert_eq!((name.as_str(), args.get("n")), ("slowest", Some(&Value::from(5))));
        assert_eq!(
            parse(br#"{"op":"tiles","build":3,"t0_ns":-5,"t1_ns":100,"bucket_ns":10}"#),
            Ok(Request::Tiles { build: 3, t0_ns: -5, t1_ns: 100, bucket_ns: 10, lane: Lane::Thread })
        );
        assert_eq!(
            parse(br#"{"op":"tiles","build":3,"t0_ns":0,"t1_ns":100,"bucket_ns":10,"lane":"job"}"#),
            Ok(Request::Tiles { build: 3, t0_ns: 0, t1_ns: 100, bucket_ns: 10, lane: Lane::Job })
        );
        assert_eq!(
            parse(br#"{"op":"world","path":"/query?expr=Function"}"#),
            Ok(Request::World { path: "/query?expr=Function".to_owned() })
        );
    }

    #[test]
    fn an_unknown_op_and_bad_arguments_are_told_apart() {
        let unknown = parse(br#"{"op":"live"}"#).unwrap_err();
        assert_eq!(unknown.kind, Kind::UnknownOp);
        assert_eq!(unknown.detail["op"], "live");
        for bad in [
            &br#"{"op":"spans","build":1,"t0_ns":0}"#[..],
            br#"{"op":"builds","extra":1}"#,
            br#"{"op":"hello"}"#,
            br#"{"op":"spans","build":-1,"t0_ns":0,"t1_ns":1}"#,
            br#"{"op":7}"#,
            br#"not json"#,
        ] {
            let error = parse(bad).unwrap_err();
            assert_eq!(error.kind, Kind::BadArgs, "{}: {error}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn an_error_header_has_its_kind_message_and_detail() {
        let header = WireError::new(Kind::Sql, "no such table").with("query", "select * from x").header();
        let error = &header["error"];
        assert_eq!(error["kind"], "sql");
        assert_eq!(error["message"], "no such table");
        assert_eq!(error["query"], "select * from x");
        assert_eq!(header.len(), 1);
    }
}
