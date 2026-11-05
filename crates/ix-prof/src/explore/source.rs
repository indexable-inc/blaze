//! What a program serves: the [`Source`] the driver calls, and the types it answers in.
//!
//! A source says its own `hello` fields ([`Hello`]: its name and version, the clock it counts
//! on, its layers), gives each session a live feed of its own ([`Live`], ticked into a [`Tick`]),
//! and answers every request but `hello` ([`Source::op`]), which the session answers itself. A
//! source answers in tables ([`Answer`]); the session encodes them as the wire carries them.

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use serde_json::{Map, Value};

/// Set when the page stops a request: the sender is dropped as the request's stream ends
/// however it ends (answered, stopped by the page, the session gone, the server stopping), so a
/// source's long work (blaze's DuckDB query) watches it and ends. The same type as
/// blaze-profile's `sql::Stop`.
pub type Stop = crossbeam_channel::Receiver<()>;

/// A program the explorer serves.
pub trait Source: Send + Sync + 'static {
    /// The source's own fields of `hello`, asked as each session's hello is answered.
    fn hello(&self) -> Hello;

    /// A live feed for one session, made as its live stream opens: each session reads from its
    /// own place in the source's records, so two pages never take each other's rows.
    fn live(&self) -> Box<dyn Live>;

    /// The answer to op `name` (any op but `hello`), `args` the request as the page sent it (a
    /// JSON object, `op` among its fields), or `None` when the source has no such op, which the
    /// session answers `unknown_op` naming it. Called on the blocking pool: it may wait, and
    /// should end soon after `stop` says to.
    fn op(&self, name: &str, args: &Value, stop: &Stop) -> Option<Result<Answer, OpError>>;
}

/// One session's live feed.
pub trait Live: Send + 'static {
    /// The rows since this feed's last tick, into `into`: the driver has cleared it
    /// ([`Tick::clear`], `at_ns` its clock's now), and the source fills its tables
    /// ([`Tick::table`]) and may stamp `at_ns` with its own clock's reading. Called on the
    /// blocking pool every live period. An error is sent as the tick's one frame, and the
    /// tables are not.
    fn tick(&mut self, at_ns: i64, into: &mut Tick) -> Result<(), OpError>;
}

/// The source's fields of `hello`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// `daemon`: the program and its version, as the page shows them (the blaze daemon's
    /// version).
    pub daemon: String,
    /// `schema`: the version of the tables the page reads, which its gate checks
    /// (`https://ix.dev/explore/schemas.json`'s `reads`): the `blaze` layer's for the daemon,
    /// until every released daemon sends `protocol` and `layers`.
    pub schema: u32,
    /// `clock`: the clock every `_ns` of the source counts on.
    pub clock: &'static str,
    /// `now_ns`: now on that clock, as hello is answered.
    pub now_ns: i64,
    /// `source.kind`: what kind of program this is (`blaze` for the daemon).
    pub kind: &'static str,
    /// `source.extensions`: the views the page shows beside the generic ones.
    pub extensions: &'static [&'static str],
    /// `layers`: each layer on top of the core and its version.
    pub layers: &'static [Layer],
}

/// A layer on top of the core: its tables' columns, its ops and their version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub name: &'static str,
    pub version: u32,
}

/// How the page applies a live table's rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Each frame's rows add to what the page has (`spans`, `counters`).
    Append,
    /// Each frame's rows are the whole set, replacing the last (`open`): the session sends an
    /// emptied set once, as a frame of no rows.
    Replace,
    /// Each frame's rows replace the page's rows of the same key, whole, and add the others
    /// (`tasks`, by process and task): a frame carries only the rows that changed, and the page
    /// evicts a row by the table's own rule (a settled task 60 s after its `end_ns`, on the
    /// source's `at_ns`), so an empty frame is never sent.
    Upsert,
}

/// One tick of a session's live feed: each table's next bytes of its Arrow IPC stream for the
/// session, and what the feed missed.
#[derive(Debug, Default)]
pub struct Tick {
    /// When it was read: the driver's clock unless the source stamps its own.
    pub at_ns: i64,
    /// Rows the source wrote over before this feed read them, since its last tick.
    pub skipped: u64,
    /// Rows the source never recorded, since it began.
    pub lost: u64,
    /// Each table the feed has sent, in the order it first filled them; a frame goes out per
    /// table in this order.
    pub tables: Vec<LiveTable>,
}

/// A live table's part of a [`Tick`].
#[derive(Debug)]
pub struct LiveTable {
    pub name: &'static str,
    pub rule: Rule,
    /// Its rows this tick; its bytes are empty exactly when they are 0.
    pub rows: usize,
    /// The next messages of the table's IPC stream for the session ([`super::frame`]).
    pub ipc: Vec<u8>,
}

impl Tick {
    /// Empties every table, keeping its buffer, for the tick at `at_ns`.
    pub fn clear(&mut self, at_ns: i64) {
        (self.at_ns, self.skipped, self.lost) = (at_ns, 0, 0);
        for table in &mut self.tables {
            table.rows = 0;
            table.ipc.clear();
        }
    }

    /// Table `name`, added (after the others) the first time it is asked for.
    pub fn table(&mut self, name: &'static str, rule: Rule) -> &mut LiveTable {
        let at = match self.tables.iter().position(|it| it.name == name) {
            Some(at) => at,
            None => {
                // alloc: once a table a session, its buffer reused every tick
                self.tables.push(LiveTable { name, rule, rows: 0, ipc: Vec::new() });
                self.tables.len() - 1
            }
        };
        let table = &mut self.tables[at];
        table.rule = rule;
        table
    }
}

/// A table of an answer: its schema and its batches, all of it.
#[derive(Debug)]
pub struct Table {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

impl Table {
    pub fn rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    /// The table of `batches`, its schema the first's; `None` when there are none.
    pub fn of(batches: Vec<RecordBatch>) -> Option<Table> {
        // clone: an `Arc` of the schema
        let schema = batches.first()?.schema();
        Some(Table { schema, batches })
    }
}

/// An answer: its header (`op` and what else the op says) and its table, if it has one. The
/// session adds `rows` (when it has a table) and `elapsed_ns`.
#[derive(Debug)]
pub struct Answer {
    pub header: Map<String, Value>,
    pub table: Option<Table>,
}

impl Answer {
    /// The answer of op `op`, a header of `op` alone and no table.
    pub fn new(op: &'static str) -> Answer {
        let mut header = Map::new();
        // alloc: once a request, its header
        header.insert("op".to_owned(), op.into());
        Answer { header, table: None }
    }

    /// Adds `key` to the header.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Answer {
        // alloc: once a request, its header
        self.header.insert(key.to_owned(), value.into());
        self
    }
}

/// The kinds of error the core answers itself; a source has its own beside them (blaze's
/// `sql`, `no_such_build`, ...), each a `snake_case` name the page tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// No such op: not `hello` and not one of the source's.
    UnknownOp,
    BadArgs,
    /// The hello's token is not this server's: the session is closed
    /// ([`super::session::CLOSE_BAD_TOKEN`], `bad_token`).
    BadToken,
    /// A request other than hello before a good hello.
    Unauthenticated,
    /// The session has as many requests in flight as it may.
    Overloaded,
    /// The answer could not be encoded as Arrow IPC.
    Encode,
    /// The page stopped the request before it was answered.
    Cancelled,
    /// `core`: the server serves no explorer core; the message says where it looked and how to
    /// make one.
    NoCore,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::UnknownOp => "unknown_op",
            Kind::BadArgs => "bad_args",
            Kind::BadToken => "bad_token",
            Kind::Unauthenticated => "unauthenticated",
            Kind::Overloaded => "overloaded",
            Kind::Encode => "encode",
            Kind::Cancelled => "cancelled",
            Kind::NoCore => "no_core",
        }
    }
}

impl From<Kind> for &'static str {
    fn from(kind: Kind) -> &'static str {
        kind.name()
    }
}

/// An answer that failed: its kind, a sentence for a person, and typed detail for the page.
#[derive(Debug, PartialEq)]
pub struct OpError {
    /// The kind's `snake_case` name ([`Kind::name`], or the source's).
    pub kind: &'static str,
    pub message: String,
    pub detail: Map<String, Value>,
}

impl OpError {
    pub fn new(kind: impl Into<&'static str>, message: impl Into<String>) -> OpError {
        OpError { kind: kind.into(), message: message.into(), detail: Map::new() }
    }

    /// Adds `key` to the detail.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> OpError {
        // alloc: an error path
        self.detail.insert(key.to_owned(), value.into());
        self
    }

    /// The header an error is sent as: `{"error": {"kind", "message", ...detail}}`.
    pub fn header(&self) -> Map<String, Value> {
        let mut error = Map::new();
        // alloc: an error path
        error.insert("kind".to_owned(), self.kind.into());
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

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for OpError {}
