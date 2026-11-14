//! What can go wrong, each naming what it is about: the message type and the state for a
//! protocol error, the parameter or column and both types for a mismatch, the rule for a
//! refusal.

#![blaze::sans_io]

use core::fmt::{self, Write as _};

use crate::codec::{EncodeError, Oid, ValueError};
use crate::proto::backend_name;
use crate::scram::ITERATIONS_MOST;

/// A query, named by its caller: unique among the queries in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QueryId(pub u64);

impl fmt::Display for QueryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query {}", self.0)
    }
}

/// A prepared statement, named by its caller: unique among the statements a connection holds.
/// The server knows it as `blaze_<n>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatementId(pub u64);

impl fmt::Display for StatementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "statement {}", self.0)
    }
}

/// One round of the extended protocol, each ending at its `Sync`'s `ReadyForQuery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    /// `Parse`, `Describe` and `Sync` of a statement.
    Prepare(StatementId),
    /// `Bind`, `Execute` and `Sync` of a query on a prepared statement.
    Query(QueryId),
    /// `Close` and `Sync` of a statement.
    Close(StatementId),
}

impl fmt::Display for Round {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Round::Prepare(statement) => write!(f, "the prepare of {statement}"),
            Round::Query(id) => write!(f, "{id}"),
            Round::Close(statement) => write!(f, "the close of {statement}"),
        }
    }
}

/// Text the server sent, borrowed from the connection. It was checked to be UTF-8 when it
/// arrived (the connection asks for `client_encoding=UTF8`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Text<'a>(pub &'a [u8]);

impl<'a> Text<'a> {
    pub fn bytes(&self) -> &'a [u8] {
        self.0
    }

    pub fn as_str(&self) -> Option<&'a str> {
        core::str::from_utf8(self.0).ok()
    }
}

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_char(char::REPLACEMENT_CHARACTER)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

/// An `ErrorResponse` or `NoticeResponse`: its severity (`S`, as the server localised it), its
/// SQLSTATE code (`C`) and its message (`M`), the three fields the protocol always sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerError<'a> {
    pub severity: Text<'a>,
    pub code: Text<'a>,
    pub message: Text<'a>,
}

impl fmt::Display for ServerError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.severity, self.code, self.message)
    }
}

/// Where in a round's answer the connection was. A prepare goes through `ParseComplete`,
/// `ParameterDescription` and `RowDescription`; a query through `BindComplete` and `Rows`; a
/// close through `CloseComplete`; each then awaits its `ReadyForQuery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    ParseComplete,
    ParameterDescription,
    RowDescription,
    BindComplete,
    CloseComplete,
    /// Rows, or the `CommandComplete` that ends them.
    Rows,
    /// After `CommandComplete`: the `ReadyForQuery` of its `Sync`.
    ReadyForQuery,
    /// Failed: everything up to its `ReadyForQuery` is dropped.
    Skipping,
}

/// The connection's state when a message came, for protocol errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Startup sent, awaiting the server's authentication request.
    Authenticating,
    /// SCRAM client-first sent, awaiting the server-first-message.
    ScramContinue,
    /// SCRAM client-final sent, awaiting the server-final-message.
    ScramFinal,
    /// Credentials sent, awaiting `AuthenticationOk`.
    AuthenticationOk,
    /// Authenticated, awaiting the parameters and the first `ReadyForQuery`.
    Starting,
    /// Ready, no round in flight.
    Idle,
    Round {
        round: Round,
        stage: Stage,
    },
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            State::Authenticating => f.write_str("the connection awaits an authentication request"),
            State::ScramContinue => f.write_str(
                "the connection awaits SCRAM's server-first-message (AuthenticationSASLContinue)",
            ),
            State::ScramFinal => f.write_str(
                "the connection awaits SCRAM's server-final-message (AuthenticationSASLFinal)",
            ),
            State::AuthenticationOk => f.write_str("the connection awaits AuthenticationOk"),
            State::Starting => f.write_str("the connection awaits its first ReadyForQuery"),
            State::Idle => f.write_str("no query is in flight"),
            State::Round { round, stage } => {
                let awaits = match stage {
                    Stage::ParseComplete => "awaits ParseComplete",
                    Stage::ParameterDescription => "awaits ParameterDescription",
                    Stage::RowDescription => "awaits RowDescription or NoData",
                    Stage::BindComplete => "awaits BindComplete",
                    Stage::CloseComplete => "awaits CloseComplete",
                    Stage::Rows => "awaits DataRow or CommandComplete",
                    Stage::ReadyForQuery => "awaits ReadyForQuery",
                    Stage::Skipping => "failed and skips to its ReadyForQuery",
                };
                write!(f, "{round} {awaits}")
            }
        }
    }
}

/// What was wrong with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// The state does not allow this message type.
    NotAllowed,
    /// A length field below 4, the length of the field itself.
    Length {
        len: u32,
    },
    /// A length past the most a message may be now.
    TooLong {
        len: u32,
        most: usize,
    },
    Truncated {
        field: &'static str,
    },
    Unterminated {
        field: &'static str,
    },
    Trailing {
        bytes: usize,
    },
    NotUtf8 {
        field: &'static str,
    },
    /// An `ErrorResponse` or `NoticeResponse` without this field.
    MissingField {
        field: u8,
    },
    /// A `ReadyForQuery` status other than `I`, `T` or `E`.
    ReadyStatus {
        byte: u8,
    },
    /// A `CommandComplete` tag whose count is not a number.
    Tag,
    /// An authentication request (by its code) the state does not allow.
    AuthCode {
        code: u32,
    },
    /// A `BackendKeyData` body of other than 8 bytes.
    KeyLength {
        len: usize,
    },
    /// A `ParameterStatus` saying `client_encoding` is not `UTF8`.
    Encoding,
}

/// A message the protocol, or the connection's state, does not allow. It closes the
/// connection: after it nothing the server sends can be trusted to be framed or ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    /// The message's type byte.
    pub message: u8,
    pub state: State,
    pub what: What,
}

struct Tag(u8);

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_ascii_graphic() {
            write!(f, "'{}'", self.0 as char)
        } else {
            write!(f, "0x{:02x}", self.0)
        }
    }
}

fn field_name(field: u8) -> &'static str {
    match field {
        b'S' => "severity",
        b'C' => "SQLSTATE code",
        b'M' => "message",
        _ => "required",
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "protocol error: the server's {} ({}) ",
            backend_name(self.message),
            Tag(self.message)
        )?;
        match self.what {
            What::NotAllowed => f.write_str("is not allowed")?,
            What::Length { len } => {
                write!(f, "says length {len}, below the 4 bytes of the length itself")?
            }
            What::TooLong { len, most } => write!(f, "says length {len}, past the most {most}")?,
            What::Truncated { field } => write!(f, "ends inside its {field}")?,
            What::Unterminated { field } => write!(f, "has no NUL ending its {field}")?,
            What::Trailing { bytes } => write!(f, "has {bytes} bytes after its last field")?,
            What::NotUtf8 { field } => write!(f, "has a {field} that is not UTF-8")?,
            What::MissingField { field } => write!(
                f,
                "has no {} field ({}), which the protocol always sends",
                field_name(field),
                Tag(field)
            )?,
            What::ReadyStatus { byte } => {
                write!(f, "has transaction status {} (the protocol has I, T and E)", Tag(byte))?
            }
            What::Tag => f.write_str("has a command tag whose row count is not a number")?,
            What::AuthCode { code } => write!(f, "asks for authentication {code}")?,
            What::KeyLength { len } => {
                write!(f, "has a {len}-byte body (protocol 3.0 sends 8)")?
            }
            What::Encoding => f.write_str(
                "says client_encoding is not UTF8, which the connection asked for and reads text as",
            )?,
        }
        write!(f, " while {}", self.state)
    }
}

/// Why authentication cannot go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// A method this client refuses by name: MD5 (a broken hash), Kerberos V5, GSSAPI, SSPI,
    /// SCM credentials.
    Method { method: &'static str },
    /// An authentication code the protocol does not have.
    UnknownMethod { code: u32 },
    /// SASL without SCRAM-SHA-256 among its mechanisms.
    NoScram,
    /// SASL offering only SCRAM-SHA-256-PLUS: channel binding is not spoken (no TLS here).
    ChannelBindingOnly,
    /// Byte `at` of the password is not printable ASCII.
    PasswordNotPrintable { at: usize },
    /// The server-first-message is not one: what is wrong with it.
    ServerFirst { what: &'static str },
    /// The server's nonce does not extend ours.
    Nonce,
    /// An iteration count of 0 or past [`ITERATIONS_MOST`].
    Iterations { iterations: u64 },
    /// The server-final-message is not one: what is wrong with it.
    ServerFinal { what: &'static str },
    /// The server-final-message is `e=`: the server refused the proof.
    ServerRefused,
    /// The server's signature is not the one the password gives: it does not know the
    /// password, so it is not the server.
    Signature,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AuthError::Method { method } => write!(
                f,
                "the server asks for {method} authentication, which this client refuses: it speaks \
                 SCRAM-SHA-256 and cleartext passwords"
            ),
            AuthError::UnknownMethod { code } => {
                write!(f, "the server asks for authentication method {code}, which the protocol does not have")
            }
            AuthError::NoScram => f.write_str(
                "the server offers SASL without SCRAM-SHA-256, the one mechanism this client speaks",
            ),
            AuthError::ChannelBindingOnly => f.write_str(
                "the server offers only SCRAM-SHA-256-PLUS: channel binding needs TLS, which this \
                 connection does not have",
            ),
            AuthError::PasswordNotPrintable { at } => write!(
                f,
                "byte {at} of the password is not printable ASCII (0x20 to 0x7e): SASLprep (RFC \
                 4013), which a password outside it needs, is not implemented"
            ),
            AuthError::ServerFirst { what } => write!(f, "SCRAM's server-first-message {what}"),
            AuthError::Nonce => f.write_str(
                "SCRAM's server-first-message has a nonce that does not extend the client's (RFC 5802)",
            ),
            AuthError::Iterations { iterations } => write!(
                f,
                "SCRAM's server-first-message asks for {iterations} iterations (1 to {ITERATIONS_MOST})"
            ),
            AuthError::ServerFinal { what } => write!(f, "SCRAM's server-final-message {what}"),
            AuthError::ServerRefused => {
                f.write_str("SCRAM's server-final-message is an error: the server refused the proof")
            }
            AuthError::Signature => f.write_str(
                "SCRAM's server signature is not the password's: the server does not know the password",
            ),
        }
    }
}

/// Why a prepare, a query or a close cannot be sent. Nothing of it was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Before [`crate::Event::Ready`].
    NotReady,
    /// After [`crate::Event::Close`].
    Closed,
    /// Another query in flight has this id.
    IdInFlight { id: QueryId },
    /// As many rounds as the pipeline holds are in flight.
    PipelineFull { most: usize },
    /// Its deadline is not after the clock.
    DeadlinePassed { deadline: u64, now: u64 },
    /// Its text has a NUL at this byte: the protocol's strings end at one.
    TextHasNul { at: usize },
    /// More parameters than the protocol's `int16` count holds.
    TooManyParams { count: usize },
    /// More result columns than the protocol's `int16` count holds.
    TooManyColumns { count: usize },
    /// Parameter `index` (from 0; `$index+1` in the text) cannot be bound.
    Param { index: usize, why: EncodeError },
    /// The connection holds this statement already (prepared, preparing or failed): close it
    /// first.
    StatementExists { statement: StatementId },
    /// The connection holds no such statement (never prepared, or closed).
    NotPrepared { statement: StatementId },
    /// The statement's prepare has not ended: query it after `Event::Prepared`.
    StatementPreparing { statement: StatementId },
    /// The statement's prepare failed (`Event::PrepareFailed`): it is never executed.
    StatementFailed { statement: StatementId },
    /// The query binds `got` values; the statement was prepared with `expected` parameters.
    ValueCount { expected: usize, got: usize },
    /// Value `index` is of type `got`; the statement's parameter is `expected`.
    ValueType { index: usize, expected: u32, got: u32 },
    /// One of its messages (the `Parse` with the text, or the `Bind`) is past [`crate::MESSAGE_MOST`].
    TooLong { message: &'static str, len: usize },
    /// Its expected columns' names come to `len` bytes, more than a `RowDescription` can hold.
    NamesTooLong { len: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refused::NotReady => f.write_str("the connection is not ready: wait for Event::Ready"),
            Refused::Closed => f.write_str("the connection is closed"),
            Refused::IdInFlight { id } => {
                write!(f, "{id} is already in flight: ids are unique among those")
            }
            Refused::PipelineFull { most } => {
                write!(f, "{most} rounds are in flight, the most a connection pipelines")
            }
            Refused::DeadlinePassed { deadline, now } => {
                write!(f, "its deadline {deadline} is not after the clock, {now}")
            }
            Refused::TextHasNul { at } => {
                write!(f, "its text has a NUL at byte {at}, where the protocol's strings end")
            }
            Refused::TooManyParams { count } => {
                write!(f, "it binds {count} parameters, past the protocol's 65535")
            }
            Refused::TooManyColumns { count } => {
                write!(f, "it expects {count} columns, past the protocol's 65535")
            }
            Refused::Param { index, why } => write!(f, "parameter ${}: {why}", index + 1),
            Refused::StatementExists { statement } => {
                write!(f, "{statement} is held already: close it before preparing it again")
            }
            Refused::NotPrepared { statement } => {
                write!(f, "{statement} is not prepared on this connection (or is closed)")
            }
            Refused::StatementPreparing { statement } => {
                write!(f, "{statement}'s prepare has not ended: query it after Event::Prepared")
            }
            Refused::StatementFailed { statement } => {
                write!(f, "{statement}'s prepare failed, so it is never executed")
            }
            Refused::ValueCount { expected, got } => {
                write!(f, "it binds {got} values, and its statement takes {expected} parameters")
            }
            Refused::ValueType { index, expected, got } => write!(
                f,
                "value ${} is {}, and its statement's parameter is {}",
                index + 1,
                Oid(got),
                Oid(expected)
            ),
            Refused::TooLong { message, len } => write!(
                f,
                "its {message} message is {len} bytes, past the most {}",
                crate::MESSAGE_MOST
            ),
            Refused::NamesTooLong { len } => write!(
                f,
                "its expected columns' names come to {len} bytes, more than a RowDescription holds ({})",
                crate::MESSAGE_MOST
            ),
        }
    }
}

/// What a query expected to get back, against what the server describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returns {
    /// Rows of this many columns.
    Rows(u16),
    /// A count (the server's `NoData`).
    Count,
}

impl fmt::Display for Returns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Returns::Rows(columns) => write!(f, "rows of {columns} columns"),
            Returns::Count => f.write_str("a count of changed rows"),
        }
    }
}

/// Why a prepare or a query failed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error<'a> {
    /// The server's `ErrorResponse`.
    Server(ServerError<'a>),
    /// The server takes `got` parameters where the query binds `expected`.
    ParamCount { expected: u16, got: u16 },
    /// Parameter `index` (from 0) is bound as `expected`, and the server takes `got`.
    ParamType { index: u16, expected: u32, got: u32 },
    /// The query expects `expected`; the server describes `got`.
    Returns { expected: Returns, got: Returns },
    /// The server describes `got` columns where the query expects `expected`.
    ColumnCount { expected: u16, got: u16 },
    /// Column `index` (from 0) is named `got` where the query expects `expected`.
    ColumnName { index: u16, expected: Text<'a>, got: Text<'a> },
    /// Column `index`, `name`, is of type `got` where the query expects `expected`.
    ColumnType { index: u16, name: Text<'a>, expected: u32, got: u32 },
    /// A row has `got` values where its description has `expected` columns.
    RowWidth { expected: u16, got: u16 },
    /// A value of column `index`, `name`, is not of its type.
    Value { index: u16, name: Text<'a>, error: ValueError },
    /// The query did not end by its deadline.
    Deadline { deadline: u64, now: u64 },
    /// The connection closed before the query ended.
    Closed(Fatal<'a>),
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Server(error) => write!(f, "the server refused it: {error}"),
            Error::ParamCount { expected, got } => write!(
                f,
                "it is prepared with {expected} parameters, and the server's description of it takes {got}"
            ),
            Error::ParamType { index, expected, got } => write!(
                f,
                "parameter ${}: it is prepared as {}, and the server takes {}",
                u32::from(index) + 1,
                Oid(expected),
                Oid(got)
            ),
            Error::Returns { expected, got } => {
                write!(f, "it expects {expected}, and the server describes {got}")
            }
            Error::ColumnCount { expected, got } => {
                write!(f, "it expects {expected} columns, and the server describes {got}")
            }
            Error::ColumnName { index, expected, got } => write!(
                f,
                "column {index}: it expects `{expected}`, and the server names it `{got}`"
            ),
            Error::ColumnType { index, name, expected, got } => write!(
                f,
                "column {index}, `{name}`: it expects {}, and the server describes {}",
                Oid(expected),
                Oid(got)
            ),
            Error::RowWidth { expected, got } => {
                write!(f, "a row has {got} values, and its description {expected} columns")
            }
            Error::Value { index, name, error } => {
                write!(f, "column {index}, `{name}`: the server sent {error}")
            }
            Error::Deadline { deadline, now } => {
                write!(f, "it did not end by its deadline {deadline}; the clock is at {now}")
            }
            Error::Closed(why) => write!(f, "the connection closed before it ended: {why}"),
        }
    }
}

/// Why the connection closed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fatal<'a> {
    /// `round`'s deadline passed: the connection may hold half an answer, so it closes.
    Deadline {
        round: Round,
        deadline: u64,
        now: u64,
    },
    /// Startup (authentication to the first `ReadyForQuery`) did not end by its deadline.
    StartupDeadline {
        deadline: u64,
        now: u64,
    },
    /// The peer closed, or the socket broke, with `buffered` bytes of an unfinished message.
    PeerClosed {
        buffered: usize,
    },
    Protocol(ProtocolError),
    Auth(AuthError),
    /// The server's `ErrorResponse` outside a query or a prepare (a refused login, a shutdown,
    /// a failed close).
    Server(ServerError<'a>),
    /// The config's `field` holds a NUL, where the startup message's strings end.
    Nul {
        field: &'static str,
    },
    /// The startup message would be `len` bytes, past [`crate::STARTUP_MESSAGE_MOST`].
    StartupTooLong {
        len: usize,
    },
    /// The driver gave the core `held` bytes while its events were not taken, past `most`.
    NotDrained {
        held: usize,
        most: usize,
    },
    /// The driver said it sent `len` bytes, of `pending`.
    SentTooMuch {
        len: usize,
        pending: usize,
    },
}

impl fmt::Display for Fatal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Fatal::Deadline { round, deadline, now } => write!(
                f,
                "{round} did not end by its deadline {deadline} (the clock is at {now}), so the \
                 connection, which may hold half its answer, closed"
            ),
            Fatal::StartupDeadline { deadline, now } => {
                write!(f, "startup did not end by its deadline {deadline}; the clock is at {now}")
            }
            Fatal::PeerClosed { buffered: 0 } => f.write_str("the server closed the connection"),
            Fatal::PeerClosed { buffered } => write!(
                f,
                "the server closed the connection inside a message, {buffered} bytes of it received"
            ),
            Fatal::Protocol(error) => write!(f, "{error}"),
            Fatal::Auth(error) => write!(f, "authentication failed: {error}"),
            Fatal::Server(error) => write!(f, "the server ended the connection: {error}"),
            Fatal::Nul { field } => {
                write!(
                    f,
                    "the config's {field} holds a NUL, where the startup message's strings end"
                )
            }
            Fatal::StartupTooLong { len } => write!(
                f,
                "the startup message would be {len} bytes, past the most {}",
                crate::STARTUP_MESSAGE_MOST
            ),
            Fatal::NotDrained { held, most } => write!(
                f,
                "the driver gave the connection {held} bytes while its events were not taken, past \
                 the most {most}: drain next_event after every call"
            ),
            Fatal::SentTooMuch { len, pending } => {
                write!(f, "the driver said it sent {len} bytes of the {pending} pending")
            }
        }
    }
}
