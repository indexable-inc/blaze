//! [`Connection`]: one Postgres connection as a synchronous state machine on an injected clock.
//! The driver contract is in the crate's documentation.
//!
//! The connection writes its startup message as it is made, answers the server's
//! authentication request (cleartext or SCRAM-SHA-256), reads the parameters and the backend
//! key, and says [`Event::Ready`] at the first `ReadyForQuery`. Then it speaks three rounds of
//! the extended protocol, each written whole at once and ending at its `Sync`'s
//! `ReadyForQuery`:
//!
//! - [`Connection::prepare`]: `Parse` under the statement's server-side name (`blaze_<id>`,
//!   with each parameter's type), `Describe` of the statement, `Sync`. The server's
//!   `ParameterDescription` and `RowDescription` (or `NoData`) are compared with what the
//!   caller expects: the parameters' count and types, then whether it returns rows, the
//!   columns' count, names and types. A difference fails the prepare naming the parameter or
//!   column and both types, and the statement is never executed: a query on it is refused.
//! - [`Connection::query`]: `Bind` of a prepared statement (every parameter and every result
//!   column in binary), `Execute` (no row limit), `Sync`. No `Describe`: each `DataRow` is
//!   checked against the prepared columns.
//! - [`Connection::close_statement`]: `Close` of the statement, `Sync`.
//!
//! Rounds pipeline: the server answers them in the order they were written. After a failure
//! (an `ErrorResponse`, a difference, a value that is not its column's type) everything the
//! server sends up to that round's `ReadyForQuery` is dropped, which is what Postgres does with
//! what it was sent up to the `Sync`: the next round's answer is then its own.
//!
//! `Done` and `Prepared` are said at the `ReadyForQuery`: the implicit transaction of a round
//! commits at its `Sync`, and a commit can still fail.
//!
//! Every byte of the server's stream is held in one buffer until the events made from it are
//! taken: an event borrows its rows, names and messages from there, so nothing is copied out.
//! The buffer drops what no event needs when what it drops is at least what it keeps, so each
//! byte moves at most a bounded number of times.

#![blaze::sans_io]

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;
use core::ops::Range;

use blaze_cast::{u64_from_usize, usize_from_u32};

use blaze_sql::desc::{Storage, ValueRef};

use crate::codec::{self, ValueError};
use crate::error::{
    AuthError, Error, Fatal, ProtocolError, QueryId, Refused, Returns, Round, ServerError, Stage,
    State, StatementId, Text, What,
};
use crate::proto::{
    self, DECIMAL, INT32_BYTES, MESSAGE_MOST, NONCE_BYTES, NONCE_CHARS, PROTOCOL_3_0, Reader,
    STARTUP_MESSAGE_MOST, base64_18, put_cstr, put_i32, put_u16, put_u32,
};
use crate::scram;

/// The most rounds (prepares, queries and closes) in flight on one connection.
pub const PIPELINE_MOST: usize = 1024;

/// The most bytes of the server's stream held for events not yet taken, beyond one message
/// still arriving: a driver that does not drain [`Connection::next_event`] closes the
/// connection here instead of growing it without end.
pub const HELD_MOST: usize = 64 << 20;

/// A message's type byte and its `int32` length: where its body starts.
const HEADER_BYTES: usize = 1 + INT32_BYTES;

const INBOX_MOST: usize = MESSAGE_MOST + HEADER_BYTES + HELD_MOST;

/// A buffer above this capacity, holding under a quarter of it, gives the rest back.
const SHRINK_ABOVE: usize = 64 << 10;

/// The fraction of its capacity below which a buffer shrinks: a quarter.
const SHRINK_BELOW: usize = 4;

/// A shrunk buffer keeps twice what it holds, so the next bytes in do not grow it at once.
const SHRINK_KEEPS: usize = 2;

/// The outbox's first room: the startup message and each SCRAM message fit.
const OUTBOX_START: usize = 128;

/// A `DataRow`'s value count, an `int16`, before its values.
const COUNT_BYTES: usize = size_of::<u16>();

/// The secret of protocol 3.0's `BackendKeyData`.
const SECRET_BYTES: usize = 4;

/// A `BackendKeyData` body: the process id, an `int32`, and the secret.
const KEY_BYTES: usize = size_of::<u32>() + SECRET_BYTES;

/// The most digits of a `CommandComplete` count read: every number of 19 digits is below
/// `u64::MAX`, so the sum cannot overflow.
const COUNT_DIGITS_MOST: usize = 19;

/// What every statement's server-side name starts with.
const NAME_PREFIX: &[u8] = b"blaze_";

/// The most digits of a statement id in decimal: `u64::MAX` has 20.
const ID_DIGITS_MOST: usize = 20;

/// The longest server-side statement name.
const SERVER_NAME_MOST: usize = NAME_PREFIX.len() + ID_DIGITS_MOST;

/// The client-first-message-bare's head, before the nonce: the user is empty (the server
/// takes the startup message's).
const BARE_HEAD: &[u8] = b"n=,r=";

/// The client-first-message-bare: its head and the nonce.
const BARE_LEN: usize = BARE_HEAD.len() + NONCE_CHARS;

/// The config's field the startup message names `user`, written once for both.
const USER: &str = "user";

/// The config's field the startup message names `database`, written once for both.
const DATABASE: &str = "database";

/// The field a row's value is read as, in errors.
const VALUE: &str = "value";

/// The `Bind` message's name, in errors.
const BIND: &str = "Bind";

/// The `Close` message's name, in errors.
const CLOSE: &str = "Close";

/// What a connection is made with.
#[derive(Clone, Copy, Debug)]
pub struct Config<'a> {
    pub user: &'a str,
    pub database: &'a str,
    /// Printable ASCII: SASLprep is not implemented, and a password it would change is refused
    /// when the server asks for it.
    pub password: &'a str,
    /// When startup (authentication to the first `ReadyForQuery`) must have ended, on the
    /// driver's clock.
    pub startup_deadline: u64,
}

/// The type of one parameter of a statement (`$1` is the first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    /// A value of this storage.
    Scalar(Storage),
    /// A one-dimensional array of this storage, as `= any($n)` takes.
    Array(Storage),
}

impl ParamType {
    /// The type's OID, as `Parse` names it and `ParameterDescription` describes it.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn oid(self) -> u32 {
        match self {
            ParamType::Scalar(
                storage @ (Storage::Boolean
                | Storage::Integer
                | Storage::Real
                | Storage::Text
                | Storage::Blob),
            ) => codec::scalar_oid(storage),
            ParamType::Array(
                storage @ (Storage::Boolean
                | Storage::Integer
                | Storage::Real
                | Storage::Text
                | Storage::Blob),
            ) => codec::array_oid(storage),
        }
    }
}

/// What binds to one parameter of a prepared statement, typed by the statement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bound<'a> {
    /// A value of a scalar parameter, or `ValueRef::Null` for a null of either kind.
    Value(ValueRef<'a>),
    /// The elements of an array parameter (each of its storage, or null).
    Array(&'a [ValueRef<'a>]),
}

/// What a statement gives back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect<'a> {
    /// Rows of these columns, in this order, each named and of this storage.
    Rows(&'a [(&'a str, Storage)]),
    /// The count of rows changed (the server describes no columns).
    Count,
}

/// The process and secret a cancel request names (`BackendKeyData`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendKey {
    pub pid: u32,
    pub secret: [u8; SECRET_BYTES],
}

/// What the connection tells its user. Each event borrows the connection until the next call.
#[derive(Debug)]
pub enum Event<'a> {
    /// Startup ended: statements may be prepared. Said once.
    Ready,
    /// `statement`'s description is the one expected: queries on it may be sent.
    Prepared { statement: StatementId },
    /// `statement`'s prepare failed: it is never executed, and holds its id until closed.
    PrepareFailed { statement: StatementId, error: Error<'a> },
    /// One row of `id`, each value checked against its column's storage. A null is allowed
    /// here; the caller checks nullability.
    Row { id: QueryId, columns: Columns<'a> },
    /// `id` ended: `changed` is the count its `CommandComplete` tag gives (`None` for a
    /// command whose tag has none). After it, nothing more of `id` comes.
    Done { id: QueryId, changed: Option<u64> },
    /// `id` failed. After it, nothing more of `id` comes.
    Failed { id: QueryId, error: Error<'a> },
    /// A `NoticeResponse`.
    Notice(ServerError<'a>),
    /// The connection is closed: the driver drops the socket. Every prepare and query not
    /// ended was failed before it, and nothing comes after it.
    Close { why: Fatal<'a> },
}

/// The values of one row.
#[derive(Clone, Copy)]
pub struct Columns<'a> {
    /// The `DataRow`'s values, after their count.
    body: &'a [u8],
    storages: &'a [Storage],
}

impl<'a> Columns<'a> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.storages.len()
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.storages.is_empty()
    }

    /// The values in column order.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn iter(&self) -> Values<'a> {
        Values { rest: self.body, storages: self.storages.iter() }
    }

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(&self, index: usize) -> Option<Result<ValueRef<'a>, ValueError>> {
        self.iter().nth(index)
    }
}

impl fmt::Debug for Columns<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for value in self.iter() {
            match value {
                Ok(value) => list.entry(&value),
                Err(error) => list.entry(&error),
            };
        }
        list.finish()
    }
}

/// The values of a row in column order. The row was checked against its columns when it
/// arrived, so every value decodes; were one not to, its error is the last item.
#[derive(Clone, Debug)]
pub struct Values<'a> {
    rest: &'a [u8],
    storages: core::slice::Iter<'a, Storage>,
}

impl<'a> Iterator for Values<'a> {
    type Item = Result<ValueRef<'a>, ValueError>;

    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn next(&mut self) -> Option<Result<ValueRef<'a>, ValueError>> {
        let storage = *self.storages.next()?;
        let value = codec::split_value(&mut self.rest, VALUE)
            .and_then(|value| codec::decode(storage, value));
        if value.is_err() {
            // An error ends the row: nothing after it is read.
            let none: &[Storage] = &[];
            self.storages = none.iter();
            self.rest = &[];
        }
        Some(value)
    }

    /// One item per column left: an error, which ends the row, empties what is left.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.storages.len(), Some(self.storages.len()))
    }
}

/// A range of the server's stream, by its offset from the stream's start.
#[derive(Clone, Copy, Debug)]
struct Span {
    at: u64,
    len: usize,
}

/// The three fields of an `ErrorResponse` or `NoticeResponse`.
#[derive(Clone, Copy, Debug)]
struct Fields {
    severity: Span,
    code: Span,
    message: Span,
}

/// An [`Error`] as the queue holds it: spans of the stream in place of borrows.
#[derive(Clone, Copy, Debug)]
enum Stored {
    Server(Fields),
    ParamCount { expected: u16, got: u16 },
    ParamType { index: u16, expected: u32, got: u32 },
    Returns { expected: Returns, got: Returns },
    ColumnCount { expected: u16, got: u16 },
    ColumnName { index: u16, got: Span },
    ColumnType { index: u16, expected: u32, got: u32 },
    RowWidth { expected: u16, got: u16 },
    Value { index: u16, error: ValueError },
    Deadline { deadline: u64, now: u64 },
    Closed(StoredFatal),
}

/// A [`Fatal`] as the queue holds it.
#[derive(Clone, Copy, Debug)]
enum StoredFatal {
    Deadline { round: Round, deadline: u64, now: u64 },
    StartupDeadline { deadline: u64, now: u64 },
    PeerClosed { buffered: usize },
    Protocol(ProtocolError),
    Auth(AuthError),
    Server(Fields),
    Nul { field: &'static str },
    StartupTooLong { len: usize },
    NotDrained { held: usize, most: usize },
    SentTooMuch { len: usize, pending: usize },
}

/// A statement is named in events and errors by `key`, unique for the connection's life: an id
/// closed and prepared again is another statement.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Ready,
    Notice(Fields),
    Prepared { statement: StatementId, key: u64 },
    PrepareFailed { statement: StatementId, key: u64, error: Stored },
    Row { id: QueryId, key: u64, body: Span },
    Done { id: QueryId, changed: Option<u64> },
    Failed { id: QueryId, key: u64, error: Stored },
    Close(StoredFatal),
}

/// An event not yet taken. `from` is where in the stream the message that made it starts (for
/// an event no message made, where the stream had been read to): the queue's `from`s never
/// decrease, so the front's is the first byte any event still needs.
#[derive(Clone, Copy, Debug)]
struct Pending {
    from: u64,
    kind: Kind,
}

#[derive(Clone, Copy, Debug)]
enum Auth {
    /// Awaiting the server's request.
    Request,
    /// SCRAM client-first sent.
    ScramContinue,
    /// SCRAM client-final sent: the server signature it must prove.
    ScramFinal([u8; 32]),
    /// Credentials sent or none asked: awaiting `AuthenticationOk`.
    Ok,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Auth(Auth),
    Starting,
    Ready,
    Closed,
}

/// Where a statement's prepare is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prepare {
    Preparing,
    Ready,
    Failed,
}

/// A statement the connection holds, from its `prepare` until its close's round is let go.
struct Statement<A: Allocator> {
    id: StatementId,
    key: u64,
    prepare: Prepare,
    /// `close_statement` was called: nothing more may use it, and the id may be prepared again.
    closing: bool,
    /// Each parameter's type.
    params: Box<[ParamType], A>,
    /// How many parameters: `params`' length, checked to fit the protocol's `int16`.
    param_count: u16,
    /// Whether it returns rows (else a count).
    rows: bool,
    storages: Box<[Storage], A>,
    /// How many columns: `storages`' length, checked to fit the protocol's `int16`.
    column_count: u16,
    /// The expected columns' names, joined; `name_ends` says where each ends.
    names: Box<[u8], A>,
    name_ends: Box<[usize], A>,
}

impl<A: Allocator> Statement<A> {
    fn name(&self, index: usize) -> &[u8] {
        let end = self.name_ends.get(index).copied().unwrap_or(0);
        let start = match index.checked_sub(1) {
            Some(before) => self.name_ends.get(before).copied().unwrap_or(0),
            None => 0,
        };
        self.names.get(start..end).unwrap_or(&[])
    }
}

/// A round from its call until its last event is taken.
struct InFlight {
    round: Round,
    /// The statement it is of.
    key: u64,
    /// `None` for a close, which nobody awaits.
    deadline: Option<u64>,
    stage: Stage,
    /// From `CommandComplete`, said at `ReadyForQuery`.
    changed: Option<u64>,
    /// Its last event is queued (a close has none).
    ended: bool,
    /// The server will send nothing more for it.
    answered: bool,
    /// Its last event was taken.
    told: bool,
}

/// A planted bug, for the simulation's control. Never set outside tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// A failed round ends there, as if its `ReadyForQuery` had come: what the server sends
    /// after, up to that `ReadyForQuery`, is read as the next round's.
    NoSkipToReady,
}

/// Why handling a message stopped.
enum Fail {
    /// The message is not allowed, or not well formed.
    Protocol(What),
    /// The message is well formed and ends the connection.
    Fatal(StoredFatal),
}

impl From<What> for Fail {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn from(what: What) -> Fail {
        Fail::Protocol(what)
    }
}

/// One Postgres connection: bytes and time in; bytes to send, events and the next deadline out.
pub struct Connection<A: Allocator + Copy> {
    alloc: A,
    now: u64,
    phase: Phase,
    startup_deadline: u64,
    password: Box<[u8], A>,
    /// Base64 of the caller's [`NONCE_BYTES`] random bytes: SCRAM's client nonce.
    nonce: [u8; NONCE_CHARS],
    /// The server's stream from offset `base`, read to `parsed`.
    inbox: Vec<u8, A>,
    base: u64,
    parsed: usize,
    /// What is to be sent, from `sent`.
    outbox: Vec<u8, A>,
    sent: usize,
    /// In the order sent. The first `answering` the server has answered.
    rounds: VecDeque<InFlight, A>,
    answering: usize,
    /// Sorted by `(id, key)`. An id has at most one statement not closing.
    statements: Vec<Statement<A>, A>,
    next_key: u64,
    events: VecDeque<Pending, A>,
    key: Option<BackendKey>,
    #[cfg(test)]
    plant: Option<Plant>,
}

/// The body of the message at `body` of `inbox`.
fn body_of(inbox: &[u8], body: Range<usize>) -> &[u8] {
    inbox.get(body).unwrap_or(&[])
}

/// A `ReadyForQuery` body: one byte of transaction status.
fn ready_status(body: &[u8]) -> Result<(), What> {
    let mut reader = Reader::new(body);
    match reader.u8("transaction status")? {
        b'I' | b'T' | b'E' => reader.end(),
        byte => Err(What::ReadyStatus { byte }),
    }
}

/// The count a `CommandComplete` tag gives: `INSERT 0 n`, `UPDATE n`, `DELETE n`, `SELECT n`,
/// `MERGE n`, `FETCH n`, `MOVE n`, `COPY n`; `None` for a command with no count.
fn changed_of(tag: &[u8]) -> Result<Option<u64>, What> {
    let mut words = tag.split(|&b| b == b' ');
    // The words before the count: `INSERT`'s object id (always 0 now).
    let skip = match words.next() {
        Some(b"INSERT") => 1,
        Some(b"UPDATE" | b"DELETE" | b"SELECT" | b"MERGE" | b"FETCH" | b"MOVE" | b"COPY") => 0,
        _ => return Ok(None),
    };
    // The count is the last word.
    let (Some(count), None) = (words.nth(skip), words.next()) else {
        return Err(What::Tag);
    };
    if count.is_empty() || count.len() > COUNT_DIGITS_MOST || !count.iter().all(u8::is_ascii_digit)
    {
        return Err(What::Tag);
    }
    // At most `COUNT_DIGITS_MOST` digits: below `u64::MAX`.
    Ok(Some(count.iter().fold(0u64, |n, d| n * DECIMAL + u64::from(d - b'0'))))
}

/// The statement's name on the server: [`NAME_PREFIX`] and the id in decimal, at most
/// [`SERVER_NAME_MOST`] bytes.
fn server_name(statement: StatementId) -> ([u8; SERVER_NAME_MOST], usize) {
    let mut name = [0u8; SERVER_NAME_MOST];
    let (prefix, id) = name.split_at_mut(NAME_PREFIX.len());
    prefix.copy_from_slice(NAME_PREFIX);
    let mut digits = [0u8; ID_DIGITS_MOST];
    let mut n = statement.0;
    let mut count = 0;
    // At most `ID_DIGITS_MOST` turns: `u64::MAX` has that many digits.
    loop {
        // Below 10: the low byte is the whole digit.
        let [digit, ..] = (n % DECIMAL).to_le_bytes();
        digits[count] = b'0' + digit;
        count += 1;
        n /= DECIMAL;
        if n == 0 || count == digits.len() {
            break;
        }
    }
    for (slot, digit) in id.iter_mut().zip(digits[..count].iter().rev()) {
        *slot = *digit;
    }
    (name, NAME_PREFIX.len() + count)
}

impl<A: Allocator + Copy> Connection<A> {
    /// A connection that has written its startup message. `nonce` is the caller's randomness,
    /// SCRAM's client nonce: 18 bytes from the system's random source, new for each connection.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new(config: Config<'_>, nonce: [u8; NONCE_BYTES], alloc: A) -> Connection<A> {
        let mut password = Vec::with_capacity_in(config.password.len(), alloc);
        password.extend_from_slice(config.password.as_bytes());
        let mut connection = Connection {
            alloc,
            now: 0,
            phase: Phase::Auth(Auth::Request),
            startup_deadline: config.startup_deadline,
            password: password.into_boxed_slice(),
            nonce: base64_18(&nonce),
            inbox: Vec::new_in(alloc),
            base: 0,
            parsed: 0,
            outbox: Vec::with_capacity_in(OUTBOX_START, alloc),
            sent: 0,
            rounds: VecDeque::new_in(alloc),
            answering: 0,
            statements: Vec::new_in(alloc),
            next_key: 0,
            events: VecDeque::new_in(alloc),
            key: None,
            #[cfg(test)]
            plant: None,
        };
        let refused = if config.user.as_bytes().contains(&0) {
            Some(StoredFatal::Nul { field: USER })
        } else if config.database.as_bytes().contains(&0) {
            Some(StoredFatal::Nul { field: DATABASE })
        } else {
            connection.write_startup(config).err()
        };
        if let Some(why) = refused {
            connection.close(0, why);
        }
        connection
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = Some(plant);
    }

    fn write_startup(&mut self, config: Config<'_>) -> Result<(), StoredFatal> {
        let out = &mut self.outbox;
        let at = out.len();
        put_u32(out, 0);
        put_u32(out, PROTOCOL_3_0);
        for (name, value) in
            [(USER, config.user), (DATABASE, config.database), ("client_encoding", "UTF8")]
        {
            put_cstr(out, name.as_bytes());
            put_cstr(out, value.as_bytes());
        }
        out.push(0);
        let len = out.len().saturating_sub(at);
        if len > STARTUP_MESSAGE_MOST {
            out.truncate(at);
            return Err(StoredFatal::StartupTooLong { len });
        }
        // At most `STARTUP_MESSAGE_MOST`, so this refuses nothing more.
        let Ok(field) = u32::try_from(len) else {
            out.truncate(at);
            return Err(StoredFatal::StartupTooLong { len });
        };
        if let Some(slot) = out.get_mut(at..at + INT32_BYTES) {
            slot.copy_from_slice(&field.to_be_bytes());
        }
        Ok(())
    }

    /// The time, in nanoseconds since the driver started, from a monotonic clock. A time before
    /// the last is a driver's bug; the later stays.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn advance_clock(&mut self, now: u64) {
        self.now = self.now.max(now);
        let now = self.now;
        let here = self.read_to();
        match self.phase {
            Phase::Closed => {}
            Phase::Auth(Auth::Request | Auth::ScramContinue | Auth::ScramFinal(_) | Auth::Ok)
            | Phase::Starting => {
                if now >= self.startup_deadline {
                    let deadline = self.startup_deadline;
                    self.close(here, StoredFatal::StartupDeadline { deadline, now });
                }
            }
            Phase::Ready => {
                let late = self
                    .rounds
                    .iter()
                    .position(|r| !r.ended && r.deadline.is_some_and(|d| d <= now));
                let Some(late) = late else { return };
                let Some(&InFlight { round, deadline: Some(deadline), .. }) = self.rounds.get(late)
                else {
                    return;
                };
                let why = StoredFatal::Deadline { round, deadline, now };
                // Each round not ended fails in the order it was sent: the late one naming its
                // deadline, the rest naming the close it caused.
                for index in 0..self.rounds.len() {
                    let error = if index == late {
                        Stored::Deadline { deadline, now }
                    } else {
                        Stored::Closed(why)
                    };
                    self.end_failed(index, here, error);
                }
                self.close(here, why);
            }
        }
    }

    /// Bytes the server sent: any split or join of its stream.
    pub fn received(&mut self, bytes: &[u8]) {
        self.settle();
        if matches!(self.phase, Phase::Closed) {
            return;
        }
        self.inbox.extend_from_slice(bytes);
        self.process();
        if !matches!(self.phase, Phase::Closed) && self.inbox.len() > INBOX_MOST {
            let held = self.inbox.len();
            let here = self.read_to();
            self.close(here, StoredFatal::NotDrained { held, most: INBOX_MOST });
        }
    }

    /// The peer closed the connection, or the socket broke.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn closed(&mut self) {
        if matches!(self.phase, Phase::Closed) {
            return;
        }
        let buffered = self.inbox.len().saturating_sub(self.parsed);
        let here = self.read_to();
        self.close(here, StoredFatal::PeerClosed { buffered });
    }

    /// The bytes to write next, if any; say how many were written with [`Connection::sent`].
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn next_send(&mut self) -> Option<&[u8]> {
        if matches!(self.phase, Phase::Closed) {
            return None;
        }
        match self.outbox.get(self.sent..) {
            Some(rest) if !rest.is_empty() => Some(rest),
            _ => None,
        }
    }

    /// `len` bytes of [`Connection::next_send`]'s were written.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn sent(&mut self, len: usize) {
        if matches!(self.phase, Phase::Closed) {
            return;
        }
        let pending = self.outbox.len().saturating_sub(self.sent);
        if len > pending {
            let here = self.read_to();
            self.close(here, StoredFatal::SentTooMuch { len, pending });
            return;
        }
        self.sent += len;
        if self.sent == self.outbox.len() {
            self.outbox.clear();
            self.sent = 0;
        } else if self.sent >= self.outbox.len() - self.sent {
            drop_prefix(&mut self.outbox, self.sent);
            self.sent = 0;
        }
        shrink(&mut self.outbox);
    }

    /// The earliest deadline: startup's until [`Event::Ready`], then the earliest of the
    /// prepares and queries not ended. `None` when nothing waits on the clock.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn next_deadline(&self) -> Option<u64> {
        match self.phase {
            Phase::Closed => None,
            Phase::Auth(Auth::Request | Auth::ScramContinue | Auth::ScramFinal(_) | Auth::Ok)
            | Phase::Starting => Some(self.startup_deadline),
            Phase::Ready => {
                self.rounds.iter().filter(|r| !r.ended).filter_map(|r| r.deadline).min()
            }
        }
    }

    /// The process and secret to cancel with, once the server sent them.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn backend_key(&self) -> Option<BackendKey> {
        self.key
    }

    /// The next event. What it borrows lives until the next call into the connection.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn next_event(&mut self) -> Option<Event<'_>> {
        self.settle();
        let pending = self.events.pop_front()?;
        let round = match pending.kind {
            Kind::Done { id, .. } | Kind::Failed { id, .. } => Some(Round::Query(id)),
            Kind::Prepared { statement, .. } | Kind::PrepareFailed { statement, .. } => {
                Some(Round::Prepare(statement))
            }
            Kind::Ready | Kind::Notice(..) | Kind::Row { .. } | Kind::Close(..) => None,
        };
        if let Some(round) = round
            && let Some(flight) = self.rounds.iter_mut().find(|r| r.round == round && !r.told)
        {
            flight.told = true;
        }
        Some(self.render(pending.kind))
    }

    /// The checks every round's call shares: the connection is ready, the pipeline has room.
    fn can_send(&self) -> Result<(), Refused> {
        match self.phase {
            Phase::Ready => {}
            Phase::Closed => return Err(Refused::Closed),
            Phase::Auth(Auth::Request | Auth::ScramContinue | Auth::ScramFinal(_) | Auth::Ok)
            | Phase::Starting => {
                return Err(Refused::NotReady);
            }
        }
        if self.rounds.len() >= PIPELINE_MOST {
            return Err(Refused::PipelineFull { most: PIPELINE_MOST });
        }
        Ok(())
    }

    /// The statement `id` names now (the one not closing), by its index.
    fn live(&self, id: StatementId) -> Option<usize> {
        let first = self.statements.partition_point(|s| s.id < id);
        let run = self.statements.get(first..)?;
        let within = run.iter().take_while(|s| s.id == id).position(|s| !s.closing)?;
        Some(first + within)
    }

    /// The statement of `key`, closing or not. A linear scan: the driver bounds the statements
    /// a connection holds (the driver contract, step 6), so the scan is over that bound.
    fn statement(&self, key: u64) -> Option<&Statement<A>> {
        self.statements.iter().find(|s| s.key == key)
    }

    fn statement_mut(&mut self, key: u64) -> Option<&mut Statement<A>> {
        self.statements.iter_mut().find(|s| s.key == key)
    }

    /// Prepares `text` as `statement`, to end by `deadline`: `params` are its parameters'
    /// types, `returns` what it gives back. Its outcome is [`Event::Prepared`] or
    /// [`Event::PrepareFailed`]. Refused, with nothing sent, before [`Event::Ready`], after
    /// [`Event::Close`], when the connection holds `statement` already, or when it cannot be
    /// written.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn prepare(
        &mut self,
        statement: StatementId,
        text: &str,
        params: &[ParamType],
        returns: Expect<'_>,
        deadline: u64,
    ) -> Result<(), Refused> {
        self.settle();
        self.can_send()?;
        if self.live(statement).is_some() {
            return Err(Refused::StatementExists { statement });
        }
        if deadline <= self.now {
            return Err(Refused::DeadlinePassed { deadline, now: self.now });
        }
        if let Some(at) = text.bytes().position(|b| b == 0) {
            return Err(Refused::TextHasNul { at });
        }
        let Ok(param_count) = u16::try_from(params.len()) else {
            return Err(Refused::TooManyParams { count: params.len() });
        };
        let columns: &[(&str, Storage)] = match returns {
            Expect::Rows(columns) => columns,
            Expect::Count => &[],
        };
        let Ok(column_count) = u16::try_from(columns.len()) else {
            return Err(Refused::TooManyColumns { count: columns.len() });
        };
        let names_len =
            columns.iter().fold(0usize, |sum, (name, _)| sum.saturating_add(name.len()));
        if names_len > MESSAGE_MOST {
            return Err(Refused::NamesTooLong { len: names_len });
        }
        let (name, name_len) = server_name(statement);
        let name = &name[..name_len];
        let mark = self.outbox.len();
        if let Err(refused) = write_prepare(&mut self.outbox, name, text, param_count, params) {
            self.outbox.truncate(mark);
            return Err(refused);
        }
        let mut types = Vec::with_capacity_in(params.len(), self.alloc);
        types.extend_from_slice(params);
        let mut storages = Vec::with_capacity_in(columns.len(), self.alloc);
        let mut names = Vec::with_capacity_in(names_len, self.alloc);
        let mut name_ends = Vec::with_capacity_in(columns.len(), self.alloc);
        for (name, storage) in columns {
            storages.push(*storage);
            names.extend_from_slice(name.as_bytes());
            name_ends.push(names.len());
        }
        let key = self.next_key;
        self.next_key += 1;
        // After every statement of a smaller id, and after this id's closing ones (their keys
        // are smaller).
        let at = self.statements.partition_point(|s| s.id <= statement);
        self.statements.insert(
            at,
            Statement {
                id: statement,
                key,
                prepare: Prepare::Preparing,
                closing: false,
                params: types.into_boxed_slice(),
                param_count,
                rows: matches!(returns, Expect::Rows(_)),
                storages: storages.into_boxed_slice(),
                column_count,
                names: names.into_boxed_slice(),
                name_ends: name_ends.into_boxed_slice(),
            },
        );
        self.rounds.push_back(InFlight {
            round: Round::Prepare(statement),
            key,
            deadline: Some(deadline),
            stage: Stage::ParseComplete,
            changed: None,
            ended: false,
            answered: false,
            told: false,
        });
        Ok(())
    }

    /// Executes prepared `statement` with `values` as `id`, to end by `deadline`. Refused, with
    /// nothing sent, before [`Event::Ready`], after [`Event::Close`], for a statement not
    /// prepared (or preparing, or failed, or closing), or when the values are not the
    /// statement's.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn query(
        &mut self,
        id: QueryId,
        statement: StatementId,
        values: &[Bound<'_>],
        deadline: u64,
    ) -> Result<(), Refused> {
        self.settle();
        self.can_send()?;
        if self.rounds.iter().any(|r| r.round == Round::Query(id)) {
            return Err(Refused::IdInFlight { id });
        }
        if deadline <= self.now {
            return Err(Refused::DeadlinePassed { deadline, now: self.now });
        }
        let Some(index) = self.live(statement) else {
            return Err(Refused::NotPrepared { statement });
        };
        let held = &self.statements[index];
        match held.prepare {
            Prepare::Ready => {}
            Prepare::Preparing => return Err(Refused::StatementPreparing { statement }),
            Prepare::Failed => return Err(Refused::StatementFailed { statement }),
        }
        if values.len() != held.params.len() {
            return Err(Refused::ValueCount { expected: held.params.len(), got: values.len() });
        }
        let key = held.key;
        let (name, name_len) = server_name(statement);
        let mark = self.outbox.len();
        if let Err(refused) = write_query(&mut self.outbox, &name[..name_len], &held.params, values)
        {
            self.outbox.truncate(mark);
            return Err(refused);
        }
        self.rounds.push_back(InFlight {
            round: Round::Query(id),
            key,
            deadline: Some(deadline),
            stage: Stage::BindComplete,
            changed: None,
            ended: false,
            answered: false,
            told: false,
        });
        Ok(())
    }

    /// Closes `statement` on the server (`Close`, `Sync`). Queries sent before still run; one
    /// sent after is refused, and the id may be prepared again at once. Nobody awaits a close,
    /// so it has no deadline and no event; an `ErrorResponse` to it closes the connection.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn close_statement(&mut self, statement: StatementId) -> Result<(), Refused> {
        self.settle();
        self.can_send()?;
        let Some(index) = self.live(statement) else {
            return Err(Refused::NotPrepared { statement });
        };
        let (name, name_len) = server_name(statement);
        let mark = self.outbox.len();
        if let Err(refused) = write_close(&mut self.outbox, &name[..name_len]) {
            self.outbox.truncate(mark);
            return Err(refused);
        }
        let held = &mut self.statements[index];
        held.closing = true;
        let key = held.key;
        self.rounds.push_back(InFlight {
            round: Round::Close(statement),
            key,
            deadline: None,
            stage: Stage::CloseComplete,
            changed: None,
            ended: true,
            answered: false,
            told: true,
        });
        Ok(())
    }

    /// Where the stream has been read to.
    fn read_to(&self) -> u64 {
        self.base + u64_from_usize(self.parsed)
    }

    fn push(&mut self, from: u64, kind: Kind) {
        self.events.push_back(Pending { from, kind });
    }

    /// Lets go of rounds whose last event was taken (and of a statement with its close's
    /// round: every round of it was sent before, so was let go before) and of the bytes no
    /// event needs.
    fn settle(&mut self) {
        while let Some(front) = self.rounds.front() {
            if !(front.answered && front.told) {
                break;
            }
            if let Round::Close(_) = front.round {
                let key = front.key;
                if let Some(at) = self.statements.iter().position(|s| s.key == key) {
                    self.statements.remove(at);
                }
            }
            self.rounds.pop_front();
            self.answering = self.answering.saturating_sub(1);
        }
        let keep = self.events.front().map_or(self.read_to(), |pending| pending.from);
        // Past `usize` is past the inbox: nothing to drop.
        let Ok(ahead) = usize::try_from(keep.saturating_sub(self.base)) else { return };
        let drop = ahead.min(self.parsed);
        if drop > 0 && drop >= self.inbox.len().saturating_sub(drop) {
            drop_prefix(&mut self.inbox, drop);
            self.base += u64_from_usize(drop);
            self.parsed -= drop;
            shrink(&mut self.inbox);
        }
    }

    /// Fails every round not ended naming `why`, says [`Event::Close`], and takes nothing
    /// more. `from` is where the message that caused it starts, or where the stream was read
    /// to.
    fn close(&mut self, from: u64, why: StoredFatal) {
        if matches!(self.phase, Phase::Closed) {
            return;
        }
        for index in 0..self.rounds.len() {
            self.end_failed(index, from, Stored::Closed(why));
            if let Some(flight) = self.rounds.get_mut(index) {
                flight.answered = true;
            }
        }
        self.answering = self.rounds.len();
        self.push(from, Kind::Close(why));
        self.phase = Phase::Closed;
        self.outbox.clear();
        self.sent = 0;
        shrink(&mut self.outbox);
    }

    /// Ends round `index`, if it has not ended, with `error`: a query's `Failed`, a prepare's
    /// `PrepareFailed` (its statement is never executed). A close has no event.
    fn end_failed(&mut self, index: usize, from: u64, error: Stored) {
        let Some(flight) = self.rounds.get_mut(index) else { return };
        if flight.ended {
            return;
        }
        flight.ended = true;
        let (round, key) = (flight.round, flight.key);
        match round {
            Round::Query(id) => self.push(from, Kind::Failed { id, key, error }),
            Round::Prepare(statement) => {
                if let Some(held) = self.statement_mut(key) {
                    held.prepare = Prepare::Failed;
                }
                self.push(from, Kind::PrepareFailed { statement, key, error });
            }
            Round::Close(_) => {}
        }
    }

    fn state(&self) -> State {
        match self.phase {
            Phase::Auth(Auth::Request) => State::Authenticating,
            Phase::Auth(Auth::ScramContinue) => State::ScramContinue,
            Phase::Auth(Auth::ScramFinal(_)) => State::ScramFinal,
            Phase::Auth(Auth::Ok) => State::AuthenticationOk,
            Phase::Starting => State::Starting,
            Phase::Ready | Phase::Closed => match self.rounds.get(self.answering) {
                Some(flight) => State::Round { round: flight.round, stage: flight.stage },
                None => State::Idle,
            },
        }
    }

    /// Reads every whole message of the inbox. Each turn reads one message or stops, so it
    /// ends with the bytes.
    fn process(&mut self) {
        while !matches!(self.phase, Phase::Closed) {
            let avail = self.inbox.get(self.parsed..).unwrap_or(&[]);
            let Some((&tag, rest)) = avail.split_first() else { break };
            let Some(len) = rest.first_chunk::<INT32_BYTES>() else { break };
            let len = u32::from_be_bytes(*len);
            let size = usize_from_u32(len);
            let from = self.read_to();
            let most = if matches!(self.phase, Phase::Ready) {
                MESSAGE_MOST
            } else {
                STARTUP_MESSAGE_MOST
            };
            let what = if size < INT32_BYTES {
                Some(What::Length { len })
            } else if size > most {
                Some(What::TooLong { len, most })
            } else {
                None
            };
            if let Some(what) = what {
                let error = ProtocolError { message: tag, state: self.state(), what };
                self.close(from, StoredFatal::Protocol(error));
                break;
            }
            // The type byte, then what the length counts.
            let total = 1 + size;
            if avail.len() < total {
                break;
            }
            let start = self.parsed + HEADER_BYTES;
            let end = self.parsed + total;
            self.parsed = end;
            let state = self.state();
            let handled = match self.phase {
                Phase::Auth(
                    auth @ (Auth::Request | Auth::ScramContinue | Auth::ScramFinal(_) | Auth::Ok),
                ) => self.on_auth(tag, from, start, end, auth),
                Phase::Starting => self.on_starting(tag, from, start, end),
                Phase::Ready => self.on_ready(tag, from, start, end),
                Phase::Closed => Ok(()),
            };
            match handled {
                Ok(()) => {}
                Err(Fail::Protocol(
                    what @ (What::NotAllowed
                    | What::Length { .. }
                    | What::TooLong { .. }
                    | What::Truncated { .. }
                    | What::Unterminated { .. }
                    | What::Trailing { .. }
                    | What::NotUtf8 { .. }
                    | What::MissingField { .. }
                    | What::ReadyStatus { .. }
                    | What::Tag
                    | What::AuthCode { .. }
                    | What::KeyLength { .. }
                    | What::Encoding),
                )) => {
                    let error = ProtocolError { message: tag, state, what };
                    self.close(from, StoredFatal::Protocol(error));
                }
                Err(Fail::Fatal(
                    why @ (StoredFatal::Deadline { .. }
                    | StoredFatal::StartupDeadline { .. }
                    | StoredFatal::PeerClosed { .. }
                    | StoredFatal::Protocol(..)
                    | StoredFatal::Auth(..)
                    | StoredFatal::Server(..)
                    | StoredFatal::Nul { .. }
                    | StoredFatal::StartupTooLong { .. }
                    | StoredFatal::NotDrained { .. }
                    | StoredFatal::SentTooMuch { .. }),
                )) => self.close(from, why),
            }
        }
    }

    /// The three required fields of the `ErrorResponse` or `NoticeResponse` whose body is at
    /// `body`, each checked to be UTF-8.
    fn fields(&self, from: u64, body: Range<usize>) -> Result<Fields, What> {
        let mut reader = Reader::new(body_of(&self.inbox, body));
        let (mut severity, mut code, mut message) = (None, None, None);
        // Each turn reads at least the field's type byte: it ends with the body.
        loop {
            let field = reader.u8("field type")?;
            if field == 0 {
                break;
            }
            let at = reader.at();
            let value = reader.cstr("field value")?;
            if core::str::from_utf8(value).is_err() {
                return Err(What::NotUtf8 { field: "field value" });
            }
            let span = Span { at: from + u64_from_usize(HEADER_BYTES + at), len: value.len() };
            match field {
                b'S' => severity = Some(span),
                b'C' => code = Some(span),
                b'M' => message = Some(span),
                // Detail, hint, position and the rest are not carried.
                _ => {}
            }
        }
        reader.end()?;
        Ok(Fields {
            severity: severity.ok_or(What::MissingField { field: b'S' })?,
            code: code.ok_or(What::MissingField { field: b'C' })?,
            message: message.ok_or(What::MissingField { field: b'M' })?,
        })
    }

    fn parameter_status(&self, body: Range<usize>) -> Result<(), What> {
        let mut reader = Reader::new(body_of(&self.inbox, body));
        let name = reader.cstr("parameter name")?;
        let value = reader.cstr("parameter value")?;
        reader.end()?;
        // The connection reads every text as UTF-8; any other encoding is refused. The other
        // parameters (the server's version, its time zone, ..) change nothing here.
        if name == b"client_encoding" && value != b"UTF8" {
            return Err(What::Encoding);
        }
        Ok(())
    }

    fn on_auth(
        &mut self,
        tag: u8,
        from: u64,
        start: usize,
        end: usize,
        auth: Auth,
    ) -> Result<(), Fail> {
        match tag {
            b'R' => {}
            b'E' => {
                let fields = self.fields(from, start..end)?;
                return Err(Fail::Fatal(StoredFatal::Server(fields)));
            }
            b'N' => {
                let fields = self.fields(from, start..end)?;
                self.push(from, Kind::Notice(fields));
                return Ok(());
            }
            _ => return Err(What::NotAllowed.into()),
        }
        let body = body_of(&self.inbox, start..end);
        let mut reader = Reader::new(body);
        let code = reader.u32("authentication code")?;
        let refuse = |method: &'static str| {
            Err(Fail::Fatal(StoredFatal::Auth(AuthError::Method { method })))
        };
        match (auth, code) {
            (Auth::Request | Auth::Ok, 0) => {
                reader.end()?;
                self.phase = Phase::Starting;
            }
            (Auth::Request, 3) => {
                reader.end()?;
                let (mark, at) = self.begin_password()?;
                let out = &mut self.outbox;
                put_cstr(out, &self.password);
                if let Err(len) = proto::finish(out, at) {
                    out.truncate(mark);
                    return Err(Fail::Fatal(StoredFatal::StartupTooLong { len }));
                }
                self.phase = Phase::Auth(Auth::Ok);
            }
            (Auth::Request, 10) => {
                let (mut scram, mut plus) = (false, false);
                // Each turn reads at least a NUL: it ends with the body.
                loop {
                    let mechanism = reader.cstr("SASL mechanism")?;
                    match mechanism {
                        b"" => break,
                        b"SCRAM-SHA-256" => scram = true,
                        b"SCRAM-SHA-256-PLUS" => plus = true,
                        _ => {}
                    }
                }
                reader.end()?;
                if !scram {
                    let error =
                        if plus { AuthError::ChannelBindingOnly } else { AuthError::NoScram };
                    return Err(Fail::Fatal(StoredFatal::Auth(error)));
                }
                let (mark, at) = self.begin_password()?;
                let out = &mut self.outbox;
                put_cstr(out, b"SCRAM-SHA-256");
                let length_at = out.len();
                put_i32(out, 0);
                scram::client_first(b"", &self.nonce, out);
                // A few dozen bytes, so this refuses nothing.
                let len = out.len().saturating_sub(length_at + INT32_BYTES);
                let Ok(field) = i32::try_from(len) else {
                    out.truncate(mark);
                    return Err(Fail::Fatal(StoredFatal::StartupTooLong { len }));
                };
                if let Some(slot) = out.get_mut(length_at..length_at + INT32_BYTES) {
                    slot.copy_from_slice(&field.to_be_bytes());
                }
                if let Err(len) = proto::finish(out, at) {
                    out.truncate(mark);
                    return Err(Fail::Fatal(StoredFatal::StartupTooLong { len }));
                }
                self.phase = Phase::Auth(Auth::ScramContinue);
            }
            (Auth::ScramContinue, 11) => {
                let server_first = reader.rest();
                let mut bare = [0u8; BARE_LEN];
                let (head, tail) = bare.split_at_mut(BARE_HEAD.len());
                head.copy_from_slice(BARE_HEAD);
                tail.copy_from_slice(&self.nonce);
                let out = &mut self.outbox;
                let mark = out.len();
                let at = proto::begin(out, b'p');
                let written =
                    scram::client_final(&self.password, &bare, server_first, &self.nonce, out);
                if written.is_err() {
                    out.truncate(mark);
                }
                let signature = written.map_err(|error| Fail::Fatal(StoredFatal::Auth(error)))?;
                if let Err(len) = proto::finish(out, at) {
                    out.truncate(mark);
                    return Err(Fail::Fatal(StoredFatal::StartupTooLong { len }));
                }
                self.phase = Phase::Auth(Auth::ScramFinal(signature));
            }
            (Auth::ScramFinal(signature), 12) => {
                let server_final = reader.rest();
                if let Err(error) = scram::server_final(server_final, &signature) {
                    return Err(Fail::Fatal(StoredFatal::Auth(error)));
                }
                self.phase = Phase::Auth(Auth::Ok);
            }
            // Whatever the state: a method refused by name, a code this state does not take,
            // or one the protocol does not have.
            (Auth::Request | Auth::ScramContinue | Auth::ScramFinal(_) | Auth::Ok, code) => {
                return match code {
                    2 => refuse("Kerberos V5"),
                    5 => refuse("MD5"),
                    6 => refuse("SCM credential"),
                    7 | 8 => refuse("GSSAPI"),
                    9 => refuse("SSPI"),
                    0 | 3 | 10 | 11 | 12 => Err(What::AuthCode { code }.into()),
                    code => Err(Fail::Fatal(StoredFatal::Auth(AuthError::UnknownMethod { code }))),
                };
            }
        }
        Ok(())
    }

    /// Checks the password is printable ASCII, as cleartext and SCRAM both need, and starts a
    /// `PasswordMessage`: where the outbox was, to drop back to, and where its length is.
    fn begin_password(&mut self) -> Result<(usize, usize), Fail> {
        if let Some(at) = scram::unprintable(&self.password) {
            return Err(Fail::Fatal(StoredFatal::Auth(AuthError::PasswordNotPrintable { at })));
        }
        let mark = self.outbox.len();
        Ok((mark, proto::begin(&mut self.outbox, b'p')))
    }

    fn on_starting(&mut self, tag: u8, from: u64, start: usize, end: usize) -> Result<(), Fail> {
        match tag {
            b'S' => self.parameter_status(start..end)?,
            b'K' => {
                let body = body_of(&self.inbox, start..end);
                if body.len() != KEY_BYTES {
                    return Err(What::KeyLength { len: body.len() }.into());
                }
                let mut reader = Reader::new(body);
                let pid = reader.u32("process id")?;
                let secret = reader.array::<SECRET_BYTES>("secret key")?;
                self.key = Some(BackendKey { pid, secret });
            }
            b'Z' => {
                ready_status(body_of(&self.inbox, start..end))?;
                self.phase = Phase::Ready;
                self.push(from, Kind::Ready);
            }
            b'E' => {
                let fields = self.fields(from, start..end)?;
                return Err(Fail::Fatal(StoredFatal::Server(fields)));
            }
            b'N' => {
                let fields = self.fields(from, start..end)?;
                self.push(from, Kind::Notice(fields));
            }
            _ => return Err(What::NotAllowed.into()),
        }
        Ok(())
    }

    fn on_ready(&mut self, tag: u8, from: u64, start: usize, end: usize) -> Result<(), Fail> {
        match tag {
            b'N' => {
                let fields = self.fields(from, start..end)?;
                self.push(from, Kind::Notice(fields));
                return Ok(());
            }
            b'S' => return Ok(self.parameter_status(start..end)?),
            _ => {}
        }
        let index = self.answering;
        let Some(&InFlight { round, key, stage, .. }) = self.rounds.get(index) else {
            // No round is being answered: an error here is the server ending the session.
            if tag == b'E' {
                let fields = self.fields(from, start..end)?;
                return Err(Fail::Fatal(StoredFatal::Server(fields)));
            }
            return Err(What::NotAllowed.into());
        };
        let body = body_of(&self.inbox, start..end);
        match (stage, tag) {
            (Stage::Skipping, b'Z') => {
                ready_status(body)?;
                self.answered(index);
                return Ok(());
            }
            (
                Stage::Skipping,
                b'1' | b't' | b'T' | b'n' | b'2' | b'D' | b'C' | b'I' | b'3' | b'E',
            ) => {
                return Ok(());
            }
            _ => {}
        }
        match round {
            Round::Prepare(statement) => {
                self.on_prepare(index, statement, key, stage, tag, from, start, end)
            }
            Round::Query(id) => self.on_query(index, id, key, stage, tag, from, start, end),
            Round::Close(_) => match (stage, tag) {
                (Stage::CloseComplete, b'3') => {
                    Reader::new(body).end()?;
                    self.set_stage(index, Stage::ReadyForQuery);
                    Ok(())
                }
                (Stage::ReadyForQuery, b'Z') => {
                    ready_status(body)?;
                    self.answered(index);
                    Ok(())
                }
                (
                    Stage::ParseComplete
                    | Stage::ParameterDescription
                    | Stage::RowDescription
                    | Stage::BindComplete
                    | Stage::CloseComplete
                    | Stage::Rows
                    | Stage::ReadyForQuery
                    | Stage::Skipping,
                    b'E',
                ) => {
                    let fields = self.fields(from, start..end)?;
                    Err(Fail::Fatal(StoredFatal::Server(fields)))
                }
                _ => Err(What::NotAllowed.into()),
            },
        }
    }

    #[expect(clippy::too_many_arguments, reason = "one message's place in one round")]
    fn on_prepare(
        &mut self,
        index: usize,
        statement: StatementId,
        key: u64,
        stage: Stage,
        tag: u8,
        from: u64,
        start: usize,
        end: usize,
    ) -> Result<(), Fail> {
        let body = body_of(&self.inbox, start..end);
        let Some(held) = self.statement(key) else { return Err(What::NotAllowed.into()) };
        match (stage, tag) {
            (
                Stage::ParseComplete
                | Stage::ParameterDescription
                | Stage::RowDescription
                | Stage::BindComplete
                | Stage::CloseComplete
                | Stage::Rows
                | Stage::ReadyForQuery
                | Stage::Skipping,
                b'E',
            ) => {
                let fields = self.fields(from, start..end)?;
                self.fail(index, from, Stored::Server(fields));
            }
            (Stage::ParseComplete, b'1') => {
                Reader::new(body).end()?;
                self.set_stage(index, Stage::ParameterDescription);
            }
            (Stage::ParameterDescription, b't') => {
                if let Some(error) = parameter_description(held, body)? {
                    self.fail(index, from, error);
                } else {
                    self.set_stage(index, Stage::RowDescription);
                }
            }
            (Stage::RowDescription, b'T') => {
                if let Some(error) = row_description(held, from, body)? {
                    self.fail(index, from, error);
                } else {
                    self.set_stage(index, Stage::ReadyForQuery);
                }
            }
            (Stage::RowDescription, b'n') => {
                Reader::new(body).end()?;
                if held.rows {
                    let columns = held.column_count;
                    let error =
                        Stored::Returns { expected: Returns::Rows(columns), got: Returns::Count };
                    self.fail(index, from, error);
                } else {
                    self.set_stage(index, Stage::ReadyForQuery);
                }
            }
            (Stage::ReadyForQuery, b'Z') => {
                ready_status(body)?;
                if let Some(held) = self.statement_mut(key) {
                    held.prepare = Prepare::Ready;
                }
                if let Some(flight) = self.rounds.get_mut(index) {
                    flight.ended = true;
                }
                self.push(from, Kind::Prepared { statement, key });
                self.answered(index);
            }
            _ => return Err(What::NotAllowed.into()),
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "one message's place in one round")]
    fn on_query(
        &mut self,
        index: usize,
        id: QueryId,
        key: u64,
        stage: Stage,
        tag: u8,
        from: u64,
        start: usize,
        end: usize,
    ) -> Result<(), Fail> {
        let body = body_of(&self.inbox, start..end);
        let Some(held) = self.statement(key) else { return Err(What::NotAllowed.into()) };
        match (stage, tag) {
            (
                Stage::ParseComplete
                | Stage::ParameterDescription
                | Stage::RowDescription
                | Stage::BindComplete
                | Stage::CloseComplete
                | Stage::Rows
                | Stage::ReadyForQuery
                | Stage::Skipping,
                b'E',
            ) => {
                let fields = self.fields(from, start..end)?;
                self.fail(index, from, Stored::Server(fields));
            }
            (Stage::BindComplete, b'2') => {
                Reader::new(body).end()?;
                self.set_stage(index, Stage::Rows);
            }
            (Stage::Rows, b'D') => {
                if let Some(error) = data_row(held, body)? {
                    self.fail(index, from, error);
                } else {
                    // The values after the count.
                    let span = Span {
                        at: from + u64_from_usize(HEADER_BYTES + COUNT_BYTES),
                        len: body.len().saturating_sub(COUNT_BYTES),
                    };
                    self.push(from, Kind::Row { id, key, body: span });
                }
            }
            (Stage::Rows, b'C') => {
                let mut reader = Reader::new(body);
                let command = reader.cstr("command tag")?;
                reader.end()?;
                let changed = changed_of(command)?;
                if let Some(flight) = self.rounds.get_mut(index) {
                    flight.changed = changed;
                    flight.stage = Stage::ReadyForQuery;
                }
            }
            (Stage::Rows, b'I') => {
                Reader::new(body).end()?;
                if let Some(flight) = self.rounds.get_mut(index) {
                    flight.changed = None;
                    flight.stage = Stage::ReadyForQuery;
                }
            }
            (Stage::ReadyForQuery, b'Z') => {
                ready_status(body)?;
                let mut changed = None;
                if let Some(flight) = self.rounds.get_mut(index) {
                    flight.ended = true;
                    changed = flight.changed;
                }
                self.push(from, Kind::Done { id, changed });
                self.answered(index);
            }
            _ => return Err(What::NotAllowed.into()),
        }
        Ok(())
    }

    fn set_stage(&mut self, index: usize, stage: Stage) {
        if let Some(flight) = self.rounds.get_mut(index) {
            flight.stage = stage;
        }
    }

    /// The server will send nothing more for round `index`, the one it was answering.
    fn answered(&mut self, index: usize) {
        if let Some(flight) = self.rounds.get_mut(index) {
            flight.answered = true;
            self.answering += 1;
        }
    }

    /// Fails round `index` and skips what the server sends for it up to its `ReadyForQuery`.
    fn fail(&mut self, index: usize, from: u64, error: Stored) {
        self.end_failed(index, from, error);
        self.set_stage(index, Stage::Skipping);
        #[cfg(test)]
        if self.plant == Some(Plant::NoSkipToReady) {
            self.answered(index);
        }
    }

    fn span(&self, span: Span) -> &[u8] {
        // Past `usize` is past the inbox: no bytes.
        let Ok(start) = usize::try_from(span.at.saturating_sub(self.base)) else { return &[] };
        let end = start.saturating_add(span.len);
        self.inbox.get(start..end).unwrap_or(&[])
    }

    fn server_error(&self, fields: Fields) -> ServerError<'_> {
        ServerError {
            severity: Text(self.span(fields.severity)),
            code: Text(self.span(fields.code)),
            message: Text(self.span(fields.message)),
        }
    }

    fn render(&self, kind: Kind) -> Event<'_> {
        match kind {
            Kind::Ready => Event::Ready,
            Kind::Notice(fields) => Event::Notice(self.server_error(fields)),
            Kind::Prepared { statement, .. } => Event::Prepared { statement },
            Kind::PrepareFailed {
                statement,
                key,
                error:
                    error @ (Stored::Server(..)
                    | Stored::ParamCount { .. }
                    | Stored::ParamType { .. }
                    | Stored::Returns { .. }
                    | Stored::ColumnCount { .. }
                    | Stored::ColumnName { .. }
                    | Stored::ColumnType { .. }
                    | Stored::RowWidth { .. }
                    | Stored::Value { .. }
                    | Stored::Deadline { .. }
                    | Stored::Closed(..)),
            } => Event::PrepareFailed { statement, error: self.error(key, error) },
            Kind::Row { id, key, body } => {
                let storages = self.statement(key).map_or(&[][..], |s| &s.storages[..]);
                Event::Row { id, columns: Columns { body: self.span(body), storages } }
            }
            Kind::Done { id, changed } => Event::Done { id, changed },
            Kind::Failed {
                id,
                key,
                error:
                    error @ (Stored::Server(..)
                    | Stored::ParamCount { .. }
                    | Stored::ParamType { .. }
                    | Stored::Returns { .. }
                    | Stored::ColumnCount { .. }
                    | Stored::ColumnName { .. }
                    | Stored::ColumnType { .. }
                    | Stored::RowWidth { .. }
                    | Stored::Value { .. }
                    | Stored::Deadline { .. }
                    | Stored::Closed(..)),
            } => Event::Failed { id, error: self.error(key, error) },
            Kind::Close(
                why @ (StoredFatal::Deadline { .. }
                | StoredFatal::StartupDeadline { .. }
                | StoredFatal::PeerClosed { .. }
                | StoredFatal::Protocol(..)
                | StoredFatal::Auth(..)
                | StoredFatal::Server(..)
                | StoredFatal::Nul { .. }
                | StoredFatal::StartupTooLong { .. }
                | StoredFatal::NotDrained { .. }
                | StoredFatal::SentTooMuch { .. }),
            ) => Event::Close { why: self.fatal(why) },
        }
    }

    fn error(&self, key: u64, error: Stored) -> Error<'_> {
        let name =
            |index: u16| Text(self.statement(key).map_or(&[][..], |s| s.name(usize::from(index))));
        match error {
            Stored::Server(fields) => Error::Server(self.server_error(fields)),
            Stored::ParamCount { expected, got } => Error::ParamCount { expected, got },
            Stored::ParamType { index, expected, got } => Error::ParamType { index, expected, got },
            Stored::Returns {
                expected: expected @ (Returns::Rows(..) | Returns::Count),
                got: got @ (Returns::Rows(..) | Returns::Count),
            } => Error::Returns { expected, got },
            Stored::ColumnCount { expected, got } => Error::ColumnCount { expected, got },
            Stored::ColumnName { index, got } => {
                Error::ColumnName { index, expected: name(index), got: Text(self.span(got)) }
            }
            Stored::ColumnType { index, expected, got } => {
                Error::ColumnType { index, name: name(index), expected, got }
            }
            Stored::RowWidth { expected, got } => Error::RowWidth { expected, got },
            Stored::Value {
                index,
                error:
                    error @ (ValueError::Length { .. }
                    | ValueError::Boolean { .. }
                    | ValueError::NotUtf8 { .. }
                    | ValueError::NegativeLength { .. }
                    | ValueError::Dimensions { .. }
                    | ValueError::NullFlag { .. }
                    | ValueError::NullNotFlagged { .. }
                    | ValueError::ElementOid { .. }
                    | ValueError::LowerBound { .. }
                    | ValueError::ArrayLength { .. }
                    | ValueError::Truncated { .. }
                    | ValueError::Trailing { .. }),
            } => Error::Value { index, name: name(index), error },
            Stored::Deadline { deadline, now } => Error::Deadline { deadline, now },
            Stored::Closed(
                why @ (StoredFatal::Deadline { .. }
                | StoredFatal::StartupDeadline { .. }
                | StoredFatal::PeerClosed { .. }
                | StoredFatal::Protocol(..)
                | StoredFatal::Auth(..)
                | StoredFatal::Server(..)
                | StoredFatal::Nul { .. }
                | StoredFatal::StartupTooLong { .. }
                | StoredFatal::NotDrained { .. }
                | StoredFatal::SentTooMuch { .. }),
            ) => Error::Closed(self.fatal(why)),
        }
    }

    fn fatal(&self, why: StoredFatal) -> Fatal<'_> {
        match why {
            StoredFatal::Deadline {
                round: round @ (Round::Prepare(..) | Round::Query(..) | Round::Close(..)),
                deadline,
                now,
            } => Fatal::Deadline { round, deadline, now },
            StoredFatal::StartupDeadline { deadline, now } => {
                Fatal::StartupDeadline { deadline, now }
            }
            StoredFatal::PeerClosed { buffered } => Fatal::PeerClosed { buffered },
            StoredFatal::Protocol(error) => Fatal::Protocol(error),
            StoredFatal::Auth(
                error @ (AuthError::Method { .. }
                | AuthError::UnknownMethod { .. }
                | AuthError::NoScram
                | AuthError::ChannelBindingOnly
                | AuthError::PasswordNotPrintable { .. }
                | AuthError::ServerFirst { .. }
                | AuthError::Nonce
                | AuthError::Iterations { .. }
                | AuthError::ServerFinal { .. }
                | AuthError::ServerRefused
                | AuthError::Signature),
            ) => Fatal::Auth(error),
            StoredFatal::Server(fields) => Fatal::Server(self.server_error(fields)),
            StoredFatal::Nul { field } => Fatal::Nul { field },
            StoredFatal::StartupTooLong { len } => Fatal::StartupTooLong { len },
            StoredFatal::NotDrained { held, most } => Fatal::NotDrained { held, most },
            StoredFatal::SentTooMuch { len, pending } => Fatal::SentTooMuch { len, pending },
        }
    }
}

/// Gives back most of a buffer's room once it holds under a quarter of it.
fn shrink<A: Allocator>(buffer: &mut Vec<u8, A>) {
    if buffer.capacity() > SHRINK_ABOVE && buffer.len() < buffer.capacity() / SHRINK_BELOW {
        buffer.shrink_to(buffer.len().saturating_mul(SHRINK_KEEPS).max(SHRINK_ABOVE));
    }
}

/// Drops the first `n` bytes of `bytes`, keeping the rest in place. `Vec::drain` needs a
/// stronger allocator bound on the pinned nightly; this needs none. `n` past the length drops
/// nothing (the callers keep `n <= len`).
fn drop_prefix<A: Allocator>(bytes: &mut Vec<u8, A>, n: usize) {
    let Some(rest) = bytes.len().checked_sub(n) else { return };
    bytes.copy_within(n.., 0);
    bytes.truncate(rest);
}

fn too_long(message: &'static str) -> impl Fn(usize) -> Refused {
    move |len| Refused::TooLong { message, len }
}

/// Writes `Sync`.
fn write_sync<A: Allocator>(out: &mut Vec<u8, A>) -> Result<(), Refused> {
    let at = proto::begin(out, b'S');
    proto::finish(out, at).map_err(too_long("Sync"))
}

/// Writes `Parse` (statement `name`), `Describe` of it and `Sync`. On an error the caller drops
/// what was written.
fn write_prepare<A: Allocator>(
    out: &mut Vec<u8, A>,
    name: &[u8],
    text: &str,
    param_count: u16,
    params: &[ParamType],
) -> Result<(), Refused> {
    let at = proto::begin(out, b'P');
    put_cstr(out, name);
    put_cstr(out, text.as_bytes());
    put_u16(out, param_count);
    for param in params {
        put_u32(out, param.oid());
    }
    proto::finish(out, at).map_err(too_long("Parse"))?;

    let at = proto::begin(out, b'D');
    out.push(b'S');
    put_cstr(out, name);
    proto::finish(out, at).map_err(too_long("Describe"))?;
    write_sync(out)
}

/// Writes `Bind` (the unnamed portal of statement `name`, `values` typed by `params`),
/// `Execute` and `Sync`. On an error the caller drops what was written.
fn write_query<A: Allocator>(
    out: &mut Vec<u8, A>,
    name: &[u8],
    params: &[ParamType],
    values: &[Bound<'_>],
) -> Result<(), Refused> {
    let at = proto::begin(out, b'B');
    put_cstr(out, b"");
    put_cstr(out, name);
    // One format code for every parameter: binary.
    put_u16(out, 1);
    put_u16(out, 1);
    // As many as the statement's parameters, checked when it was prepared, so this refuses
    // nothing more.
    let Ok(count) = u16::try_from(params.len()) else {
        return Err(Refused::TooManyParams { count: params.len() });
    };
    put_u16(out, count);
    for (index, (&param, &value)) in params.iter().zip(values).enumerate() {
        let written = match (param, value) {
            (ParamType::Scalar(..) | ParamType::Array(..), Bound::Value(ValueRef::Null)) => {
                put_i32(out, -1);
                Ok(())
            }
            (
                ParamType::Scalar(
                    storage @ (Storage::Boolean
                    | Storage::Integer
                    | Storage::Real
                    | Storage::Text
                    | Storage::Blob),
                ),
                Bound::Value(
                    value @ (ValueRef::Boolean(..)
                    | ValueRef::Integer(..)
                    | ValueRef::Real(..)
                    | ValueRef::Text(..)
                    | ValueRef::Blob(..)),
                ),
            ) if value.storage() == Some(storage) => codec::encode(value, out),
            (
                ParamType::Array(
                    storage @ (Storage::Boolean
                    | Storage::Integer
                    | Storage::Real
                    | Storage::Text
                    | Storage::Blob),
                ),
                Bound::Array(items),
            ) => codec::encode_array(storage, items, out),
            (ParamType::Scalar(..) | ParamType::Array(..), Bound::Value(..) | Bound::Array(..)) => {
                let got = match value {
                    Bound::Value(
                        value @ (ValueRef::Null
                        | ValueRef::Boolean(..)
                        | ValueRef::Integer(..)
                        | ValueRef::Real(..)
                        | ValueRef::Text(..)
                        | ValueRef::Blob(..)),
                    ) => value.storage().map_or(0, codec::scalar_oid),
                    Bound::Array(items) => {
                        items.iter().find_map(ValueRef::storage).map_or(0, codec::array_oid)
                    }
                };
                return Err(Refused::ValueType { index, expected: param.oid(), got });
            }
        };
        written.map_err(|why| Refused::Param { index, why })?;
        let len = out.len().saturating_sub(at);
        if len > MESSAGE_MOST {
            return Err(Refused::TooLong { message: BIND, len });
        }
    }
    // One format code for every result column: binary.
    put_u16(out, 1);
    put_u16(out, 1);
    proto::finish(out, at).map_err(too_long(BIND))?;

    let at = proto::begin(out, b'E');
    put_cstr(out, b"");
    put_i32(out, 0);
    proto::finish(out, at).map_err(too_long("Execute"))?;
    write_sync(out)
}

/// Writes `Close` of statement `name` and `Sync`.
fn write_close<A: Allocator>(out: &mut Vec<u8, A>, name: &[u8]) -> Result<(), Refused> {
    let at = proto::begin(out, b'C');
    out.push(b'S');
    put_cstr(out, name);
    proto::finish(out, at).map_err(too_long(CLOSE))?;
    write_sync(out)
}

/// Compares a `ParameterDescription` with the statement's parameters: the first difference, or
/// `None`.
fn parameter_description<A: Allocator>(
    held: &Statement<A>,
    body: &[u8],
) -> Result<Option<Stored>, What> {
    let mut reader = Reader::new(body);
    let count = reader.u16("parameter count")?;
    let mut mismatch = None;
    for param in 0..count {
        let got = reader.u32("parameter type")?;
        if let Some(want) = held.params.get(usize::from(param))
            && want.oid() != got
            && mismatch.is_none()
        {
            mismatch = Some(Stored::ParamType { index: param, expected: want.oid(), got });
        }
    }
    reader.end()?;
    let expected = held.param_count;
    if count != expected {
        mismatch = Some(Stored::ParamCount { expected, got: count });
    }
    Ok(mismatch)
}

/// Compares a `RowDescription` with what the statement expects: the first difference, or
/// `None`.
fn row_description<A: Allocator>(
    held: &Statement<A>,
    from: u64,
    body: &[u8],
) -> Result<Option<Stored>, What> {
    let mut reader = Reader::new(body);
    let count = reader.u16("column count")?;
    let mut mismatch = None;
    let expected = held.column_count;
    if !held.rows {
        mismatch = Some(Stored::Returns { expected: Returns::Count, got: Returns::Rows(count) });
    } else if count != expected {
        mismatch = Some(Stored::ColumnCount { expected, got: count });
    }
    for index in 0..count {
        let at = reader.at();
        let name = reader.cstr("column name")?;
        if core::str::from_utf8(name).is_err() {
            return Err(What::NotUtf8 { field: "column name" });
        }
        let _table = reader.u32("column's table")?;
        let _attribute = reader.u16("column's attribute number")?;
        let oid = reader.u32("column type")?;
        let _size = reader.u16("column's type size")?;
        let _modifier = reader.i32("column's type modifier")?;
        // A `Describe` of a statement says format 0: formats are chosen at `Bind`.
        let _format = reader.u16("column's format")?;
        if mismatch.is_some() {
            continue;
        }
        let i = usize::from(index);
        if name != held.name(i) {
            let got = Span { at: from + u64_from_usize(HEADER_BYTES + at), len: name.len() };
            mismatch = Some(Stored::ColumnName { index, got });
        } else if let Some(&storage) = held.storages.get(i)
            && codec::scalar_oid(storage) != oid
        {
            mismatch =
                Some(Stored::ColumnType { index, expected: codec::scalar_oid(storage), got: oid });
        }
    }
    reader.end()?;
    Ok(mismatch)
}

/// Checks a `DataRow` against the statement's columns: the first value that is not its
/// column's, or `None`.
fn data_row<A: Allocator>(held: &Statement<A>, body: &[u8]) -> Result<Option<Stored>, What> {
    let mut reader = Reader::new(body);
    let count = reader.u16("value count")?;
    let mut rest = reader.rest();
    let expected = held.column_count;
    if count != expected {
        return Ok(Some(Stored::RowWidth { expected, got: count }));
    }
    for (index, &storage) in (0..count).zip(held.storages.iter()) {
        let checked =
            codec::split_value(&mut rest, VALUE).and_then(|value| codec::decode(storage, value));
        if let Err(error) = checked {
            return Ok(Some(Stored::Value { index, error }));
        }
    }
    if !rest.is_empty() {
        return Err(What::Trailing { bytes: rest.len() });
    }
    Ok(None)
}
