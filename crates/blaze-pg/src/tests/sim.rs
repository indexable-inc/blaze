//! The connection against a scripted Postgres. The tape draws:
//!
//! - how the server authenticates;
//! - the statements: their parameters' types and expected columns, and how the server describes
//!   each (as expected; with a wrong parameter type, column type, column name, column count or
//!   kind of result; or with an `ErrorResponse`);
//! - each statement's queries: their values, and what the server does with each (rows, an error
//!   before or between rows, a commit that fails);
//! - for any round, a close in the middle of its answer or silence from it on;
//! - which statements are closed;
//! - how the stream is cut on its way in (down to one byte), how much of each send the socket
//!   takes, clock steps and deadlines, and the order of the asks.
//!
//! The oracle holds every event to the plan, and the scripted server holds every frontend
//! message to it: a `Bind` must name a statement it parsed whose description was the one
//! expected.

use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use alloc::alloc::Global;
use blaze_sql::desc::{Storage, ValueRef};
use blaze_sim::{Broke, Tape};

use super::server::{
    Body, Frontend, ScramServer, authentication, command_complete, data_row, message,
    parameter_description, report, row_description, startup_done,
};
use super::{Owned, storage, value};
use crate::codec;
use crate::conn::Plant;
use crate::{
    Bound, Config, Connection, Error, Event, Expect, Fatal, ParamType, QueryId, Refused,
    StatementId,
};

const USER: &str = "app";
const DATABASE: &str = "db";
const PASSWORD: &str = "s3cret pass";
/// The most drawn steps before the run is finished off.
const STEPS_MOST: usize = 300;
/// The most rounds of asking, sending and delivering everything at the end.
const ROUNDS_MOST: usize = 10_000;
const STATEMENTS_MOST: usize = 6;
const EXECUTIONS_MOST: usize = 3;

macro_rules! oracle {
    ($($arg:tt)*) => { return Err(Broke::Oracle(format!($($arg)*))) };
}

macro_rules! harness {
    ($($arg:tt)*) => { return Err(Broke::Harness(format!($($arg)*))) };
}

// ---------------------------------------------------------------------------------------------
// The plan.

#[derive(Clone, Copy, Debug)]
enum AuthPlan {
    Trust,
    Cleartext,
    /// SCRAM-SHA-256 with this many iterations; `plus` offers SCRAM-SHA-256-PLUS first.
    Scram {
        iterations: u32,
        plus: bool,
    },
}

/// How the server describes a statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Describe {
    Right,
    /// This parameter as another type.
    WrongParam(usize),
    /// This column as another type.
    WrongColumnType(usize),
    /// This column under another name.
    WrongName(usize),
    /// One column more than expected.
    WrongCount,
    /// A row for a statement that expects a count.
    WrongReturns,
    /// An `ErrorResponse` in place of `ParseComplete`: the statement is not parsed.
    Error,
}

impl Describe {
    /// The error a prepare described so fails with.
    fn mismatch(self) -> Option<&'static str> {
        Some(match self {
            Describe::WrongParam(_) => "ParamType",
            Describe::WrongColumnType(_) => "ColumnType",
            Describe::WrongName(_) => "ColumnName",
            Describe::WrongCount => "ColumnCount",
            Describe::WrongReturns => "Returns",
            Describe::Right | Describe::Error => return None,
        })
    }
}

/// How the server's answer to one round reaches the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    Whole,
    /// The server closes this far (modulo its length) into the answer.
    CloseAt(usize),
    /// The server never answers it, nor anything after it.
    Silent,
}

/// What the server does with one query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Rows,
    /// An `ErrorResponse` after this many rows.
    ErrorAt(usize),
    /// Every row and the `CommandComplete`, then an `ErrorResponse` (the commit at `Sync`).
    CommitFails,
}

/// One value bound: a scalar or a null of either kind, or an array's elements.
#[derive(Clone, Debug)]
enum Value {
    Of(Owned),
    Array(Vec<Owned>),
}

#[derive(Clone, Debug)]
struct Execution {
    values: Vec<Value>,
    rows: Vec<Vec<Owned>>,
    count: u32,
    answer: Answer,
    cut: Cut,
    notice: bool,
}

#[derive(Clone, Debug)]
struct Planned {
    text: String,
    params: Vec<ParamType>,
    /// `None`: a count.
    columns: Option<Vec<(String, Storage)>>,
    describe: Describe,
    cut: Cut,
    notice: bool,
    executions: Vec<Execution>,
    /// Closed once its queries are asked.
    close: bool,
}

#[derive(Debug)]
struct Plan {
    auth: AuthPlan,
    startup_deadline: u64,
    statements: Vec<Planned>,
}

fn draw_plan(tape: &Tape) -> Plan {
    let auth = match tape.int_in(0..=2u8, "auth") {
        0 => AuthPlan::Trust,
        1 => AuthPlan::Cleartext,
        _ => AuthPlan::Scram {
            iterations: tape.int_in(1..=8u32, "scram iterations"),
            plus: tape.bool("scram plus"),
        },
    };
    let startup_deadline = tape.int_in(1..=100_000u64, "startup deadline");
    let count = tape.int_in(0..=STATEMENTS_MOST, "statements");
    let statements =
        (0..count).map(|index| tape.span("statement", |tape| draw_statement(tape, index))).collect();
    Plan { auth, startup_deadline, statements }
}

/// A cut: mostly whole.
fn draw_cut(tape: &Tape) -> Cut {
    match tape.int_in(0..=9u8, "cut") {
        0 => Cut::CloseAt(tape.int_in(0..=4096usize, "close at")),
        1 => Cut::Silent,
        _ => Cut::Whole,
    }
}

fn draw_statement(tape: &Tape, index: usize) -> Planned {
    let param_count = tape.int_in(0..=3usize, "params");
    let params: Vec<ParamType> = (0..param_count)
        .map(|_| {
            let storage = storage(tape);
            if tape.bool("scalar param") {
                ParamType::Scalar(storage)
            } else {
                ParamType::Array(storage)
            }
        })
        .collect();
    let columns: Option<Vec<(String, Storage)>> = if tape.bool("returns rows") {
        let width = tape.int_in(0..=3usize, "width");
        Some((0..width).map(|at| (format!("c{at}"), storage(tape))).collect())
    } else {
        None
    };
    // The descriptions this statement can have; mostly the right one.
    let mut kinds: Vec<u8> = [0, 0, 0, 0, 1].to_vec();
    if !params.is_empty() {
        kinds.push(2);
    }
    match &columns {
        Some(columns) => {
            kinds.push(5);
            if !columns.is_empty() {
                kinds.extend_from_slice(&[3, 4]);
            }
        }
        None => kinds.push(6),
    }
    let width = columns.as_ref().map_or(0, Vec::len);
    let at = |len: usize| tape.int_in(0..=len.saturating_sub(1), "wrong at");
    let describe = match *tape.one_of(&kinds, "describe") {
        0 => Describe::Right,
        1 => Describe::Error,
        2 => Describe::WrongParam(at(params.len())),
        3 => Describe::WrongColumnType(at(width)),
        4 => Describe::WrongName(at(width)),
        5 => Describe::WrongCount,
        _ => Describe::WrongReturns,
    };
    let runs = tape.int_in(0..=EXECUTIONS_MOST, "executions");
    let executions = (0..runs)
        .map(|_| tape.span("execution", |tape| draw_execution(tape, &params, columns.as_deref())))
        .collect();
    Planned {
        text: format!("select {index}"),
        params,
        columns,
        describe,
        cut: draw_cut(tape),
        notice: tape.bool("notice"),
        executions,
        close: tape.bool("close"),
    }
}

fn draw_execution(
    tape: &Tape,
    params: &[ParamType],
    columns: Option<&[(String, Storage)]>,
) -> Execution {
    let values = params
        .iter()
        .map(|param| match *param {
            ParamType::Scalar(storage) => Value::Of(value(tape, storage, true)),
            ParamType::Array(storage) => {
                if tape.int_in(0..=4u8, "null array") == 0 {
                    Value::Of(Owned::Null)
                } else {
                    let len = tape.int_in(0..=3usize, "array length");
                    Value::Array((0..len).map(|_| value(tape, storage, true)).collect())
                }
            }
        })
        .collect();
    let rows: Vec<Vec<Owned>> = match columns {
        Some(columns) => {
            let height = tape.int_in(0..=4usize, "rows");
            (0..height)
                .map(|_| columns.iter().map(|(_, storage)| value(tape, *storage, true)).collect())
                .collect()
        }
        None => Vec::new(),
    };
    let answer = match tape.int_in(0..=5u8, "answer") {
        0 => Answer::ErrorAt(tape.int_in(0..=rows.len(), "error at")),
        1 => Answer::CommitFails,
        _ => Answer::Rows,
    };
    Execution {
        values,
        rows,
        count: tape.int("count"),
        answer,
        cut: draw_cut(tape),
        notice: tape.bool("notice"),
    }
}

/// Another type than `oid`, for a description that differs.
fn other_oid(oid: u32) -> u32 {
    if oid == codec::INT8 { codec::TEXT } else { codec::INT8 }
}

fn statement_id(k: usize) -> StatementId {
    StatementId(k as u64)
}

fn query_id(k: usize, j: usize) -> QueryId {
    QueryId((k * 16 + j) as u64)
}

/// A round of the plan: a statement's prepare, or its `j`th query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoundOf {
    Prepare(usize),
    Query(usize, usize),
}

// ---------------------------------------------------------------------------------------------
// The scripted server.

enum PeerStage {
    Startup,
    Password,
    SaslInitial,
    SaslResponse(ScramServer),
    Ready,
}

/// The frontend messages of the round being read, up to its `Sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    None,
    /// `Parse` of statement `k` read; `described` once its `Describe` is.
    Prepare {
        k: usize,
        described: bool,
    },
    /// `Bind` of statement `k`'s `j`th query read; `executed` once its `Execute` is.
    Query {
        k: usize,
        j: usize,
        executed: bool,
    },
    Close {
        k: usize,
    },
}

struct Peer<'p> {
    plan: &'p Plan,
    stage: PeerStage,
    frontend: Frontend,
    reading: Reading,
    /// Per statement: the server holds it parsed.
    parsed: Vec<bool>,
    /// Per statement: how many of its queries were bound.
    bound: Vec<usize>,
    /// Everything the server wrote.
    out: Vec<u8>,
    /// Where each round's whole answer ends in `out`.
    ends: Vec<(RoundOf, usize)>,
    /// Neither reads nor writes any more.
    stopped: bool,
    /// The server closes its side when the client has read this far.
    close_at: Option<usize>,
}

/// The statement a server-side name is, by plan index.
fn statement_of(name: &[u8]) -> Result<usize, String> {
    let digits = name
        .strip_prefix(b"blaze_")
        .ok_or_else(|| format!("a statement named {:?}", String::from_utf8_lossy(name)))?;
    let text =
        core::str::from_utf8(digits).map_err(|_| "a statement name not UTF-8".to_string())?;
    text.parse().map_err(|_| format!("a statement named blaze_{text}"))
}

impl<'p> Peer<'p> {
    fn new(plan: &'p Plan) -> Peer<'p> {
        Peer {
            plan,
            stage: PeerStage::Startup,
            frontend: Frontend::default(),
            reading: Reading::None,
            parsed: plan.statements.iter().map(|_| false).collect(),
            bound: plan.statements.iter().map(|_| 0).collect(),
            out: Vec::new(),
            ends: Vec::new(),
            stopped: false,
            close_at: None,
        }
    }

    fn statement(&self, k: usize) -> Result<&'p Planned, String> {
        let plan: &'p Plan = self.plan;
        plan.statements.get(k).ok_or_else(|| format!("statement {k}, which the plan does not have"))
    }

    /// Bytes the client wrote: every whole message is answered.
    fn take(&mut self, bytes: &[u8]) -> Result<(), Broke> {
        if self.stopped {
            return Ok(());
        }
        self.frontend.push(bytes);
        // Each turn reads one message from the bytes pushed, so the loop ends with them.
        loop {
            if self.stopped {
                return Ok(());
            }
            let next = self
                .frontend
                .next()
                .map_err(|why| Broke::Oracle(format!("the client wrote {why}")))?;
            let Some((tag, body)) = next else { return Ok(()) };
            self.on_message(tag, &body)?;
        }
    }

    fn on_message(&mut self, tag: u8, body: &[u8]) -> Result<(), Broke> {
        let bad =
            |why: String| Broke::Oracle(format!("the client's {:?} message: {why}", tag as char));
        let mut reader = Body(body);
        match (&self.stage, tag) {
            (PeerStage::Startup, 0) => {
                let protocol = reader.i32().map_err(bad)?;
                if protocol != 3 << 16 {
                    oracle!("the startup message asks for protocol {protocol:#x}");
                }
                let want: &[u8] = b"user\0app\0database\0db\0client_encoding\0UTF8\0\0";
                if reader.0 != want {
                    oracle!(
                        "the startup message's parameters are {:?}",
                        String::from_utf8_lossy(reader.0)
                    );
                }
                match self.plan.auth {
                    AuthPlan::Trust => {
                        authentication(&mut self.out, 0, &[]);
                        startup_done(&mut self.out);
                        self.stage = PeerStage::Ready;
                    }
                    AuthPlan::Cleartext => {
                        authentication(&mut self.out, 3, &[]);
                        self.stage = PeerStage::Password;
                    }
                    AuthPlan::Scram { plus, .. } => {
                        let offer: &[u8] = if plus {
                            b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0"
                        } else {
                            b"SCRAM-SHA-256\0\0"
                        };
                        authentication(&mut self.out, 10, offer);
                        self.stage = PeerStage::SaslInitial;
                    }
                }
            }
            (PeerStage::Password, b'p') => {
                let password = reader.cstr().map_err(bad)?;
                reader.end().map_err(bad)?;
                if password != PASSWORD.as_bytes() {
                    oracle!("the client sent the password {:?}", String::from_utf8_lossy(password));
                }
                authentication(&mut self.out, 0, &[]);
                startup_done(&mut self.out);
                self.stage = PeerStage::Ready;
            }
            (PeerStage::SaslInitial, b'p') => {
                let mechanism = reader.cstr().map_err(bad)?;
                if mechanism != b"SCRAM-SHA-256" {
                    oracle!("the client chose {:?}", String::from_utf8_lossy(mechanism));
                }
                let len = reader.i32().map_err(bad)?;
                let first = reader.take(usize::try_from(len).unwrap_or(usize::MAX)).map_err(bad)?;
                reader.end().map_err(bad)?;
                let AuthPlan::Scram { iterations, .. } = self.plan.auth else {
                    harness!("SASL without a SCRAM plan");
                };
                let server =
                    ScramServer::new(first, b"a salt of the server's", iterations).map_err(bad)?;
                authentication(&mut self.out, 11, &server.server_first);
                self.stage = PeerStage::SaslResponse(server);
            }
            (PeerStage::SaslResponse(server), b'p') => {
                let last = server.finish(PASSWORD.as_bytes(), body).map_err(bad)?;
                authentication(&mut self.out, 12, &last);
                authentication(&mut self.out, 0, &[]);
                startup_done(&mut self.out);
                self.stage = PeerStage::Ready;
            }
            (PeerStage::Ready, _) => self.on_round(tag, reader).map_err(bad)?,
            _ => oracle!("the client sent {:?} where the server did not expect it", tag as char),
        }
        Ok(())
    }

    /// One message of a round, after startup.
    fn on_round(&mut self, tag: u8, mut reader: Body<'_>) -> Result<(), String> {
        match (self.reading, tag) {
            (Reading::None, b'P') => {
                let k = statement_of(reader.cstr()?)?;
                let planned = self.statement(k)?;
                let text = reader.cstr()?;
                let count = reader.u16()?;
                let mut oids = Vec::new();
                for _ in 0..count {
                    let bytes = reader.take(4)?;
                    oids.push(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                }
                reader.end()?;
                let want: Vec<u32> = planned.params.iter().map(|param| param.oid()).collect();
                if text != planned.text.as_bytes() || oids != want {
                    return Err(format!(
                        "Parse of {:?} with OIDs {oids:?}, and statement {k} is {:?} with {want:?}",
                        String::from_utf8_lossy(text),
                        planned.text
                    ));
                }
                if self.parsed[k] {
                    return Err(format!("Parse of statement {k}, which the server holds already"));
                }
                self.reading = Reading::Prepare { k, described: false };
            }
            (Reading::Prepare { k, described: false }, b'D') => {
                if reader.take(1)? != b"S" || statement_of(reader.cstr()?)? != k {
                    return Err(format!("a Describe of other than statement {k}"));
                }
                reader.end()?;
                self.reading = Reading::Prepare { k, described: true };
            }
            (Reading::None, b'B') => {
                if !reader.cstr()?.is_empty() {
                    return Err("a Bind of a named portal".to_string());
                }
                let k = statement_of(reader.cstr()?)?;
                let planned = self.statement(k)?;
                if !self.parsed[k] {
                    return Err(format!(
                        "a Bind of statement {k}, which the server has not parsed"
                    ));
                }
                if planned.describe != Describe::Right {
                    return Err(format!(
                        "a Bind of statement {k}, whose description differs ({:?}): it was executed",
                        planned.describe
                    ));
                }
                let j = self.bound[k];
                let execution = planned
                    .executions
                    .get(j)
                    .ok_or_else(|| format!("query {j} of statement {k}, which was not asked"))?;
                check_bind(&mut reader, &planned.params, &execution.values)?;
                self.bound[k] += 1;
                self.reading = Reading::Query { k, j, executed: false };
            }
            (Reading::Query { k, j, executed: false }, b'E') => {
                let portal = reader.cstr()?;
                let most = reader.i32()?;
                reader.end()?;
                if !portal.is_empty() || most != 0 {
                    return Err(format!("an Execute of portal {portal:?} for {most} rows"));
                }
                self.reading = Reading::Query { k, j, executed: true };
            }
            (Reading::None, b'C') => {
                if reader.take(1)? != b"S" {
                    return Err("a Close of other than a statement".to_string());
                }
                let k = statement_of(reader.cstr()?)?;
                reader.end()?;
                self.statement(k)?;
                // Closing a statement the server does not hold is not an error in Postgres.
                self.parsed[k] = false;
                self.reading = Reading::Close { k };
            }
            (reading, b'S') => {
                reader.end()?;
                self.reading = Reading::None;
                match reading {
                    Reading::Prepare { k, described: true } => self.answer_prepare(k)?,
                    Reading::Query { k, j, executed: true } => self.answer_query(k, j)?,
                    Reading::Close { .. } => {
                        message(&mut self.out, b'3', &[]);
                        message(&mut self.out, b'Z', b"I");
                    }
                    other => return Err(format!("a Sync of a round not sent whole ({other:?})")),
                }
            }
            (reading, _) => return Err(format!("{:?} while reading {reading:?}", tag as char)),
        }
        Ok(())
    }

    fn answer_prepare(&mut self, k: usize) -> Result<(), String> {
        let planned = self.statement(k)?;
        if planned.cut == Cut::Silent {
            self.stopped = true;
            return Ok(());
        }
        let start = self.out.len();
        let out = &mut self.out;
        if planned.notice {
            report(out, b'N', "NOTICE", "00000", "a notice before the answer");
        }
        if planned.describe == Describe::Error {
            report(out, b'E', "ERROR", "42P01", "relation does not exist");
        } else {
            message(out, b'1', &[]);
            let mut oids: Vec<u32> = planned.params.iter().map(|param| param.oid()).collect();
            if let Describe::WrongParam(at) = planned.describe
                && let Some(oid) = oids.get_mut(at)
            {
                *oid = other_oid(*oid);
            }
            parameter_description(out, &oids);
            match &planned.columns {
                Some(columns) => {
                    let mut described: Vec<(String, u32)> = columns
                        .iter()
                        .map(|(name, storage)| (name.clone(), codec::scalar_oid(*storage)))
                        .collect();
                    match planned.describe {
                        Describe::WrongColumnType(at) => {
                            if let Some(column) = described.get_mut(at) {
                                column.1 = other_oid(column.1);
                            }
                        }
                        Describe::WrongName(at) => {
                            if let Some(column) = described.get_mut(at) {
                                column.0.push_str("_other");
                            }
                        }
                        Describe::WrongCount => described.push(("extra".to_string(), codec::INT8)),
                        _ => {}
                    }
                    let described: Vec<(&str, u32)> =
                        described.iter().map(|(n, o)| (n.as_str(), *o)).collect();
                    row_description(out, &described);
                }
                None if planned.describe == Describe::WrongReturns => {
                    row_description(out, &[("n", codec::INT8)])
                }
                None => message(out, b'n', &[]),
            }
            self.parsed[k] = true;
        }
        message(out, b'Z', b"I");
        self.finish(RoundOf::Prepare(k), planned.cut, start);
        Ok(())
    }

    fn answer_query(&mut self, k: usize, j: usize) -> Result<(), String> {
        let planned = self.statement(k)?;
        let execution =
            planned.executions.get(j).ok_or_else(|| format!("no query {j} of statement {k}"))?;
        if execution.cut == Cut::Silent {
            self.stopped = true;
            return Ok(());
        }
        let start = self.out.len();
        let out = &mut self.out;
        if execution.notice {
            report(out, b'N', "NOTICE", "00000", "a notice before the answer");
        }
        message(out, b'2', &[]);
        let rows: &[Vec<Owned>] = match execution.answer {
            Answer::ErrorAt(at) => execution.rows.get(..at).unwrap_or(&execution.rows),
            _ => &execution.rows,
        };
        for row in rows {
            data_row(out, row);
        }
        match (execution.answer, &planned.columns) {
            (Answer::ErrorAt(_), _) => report(out, b'E', "ERROR", "P0001", "raised between rows"),
            (_, Some(_)) => command_complete(out, &format!("SELECT {}", rows.len())),
            (_, None) => command_complete(out, &format!("UPDATE {}", execution.count)),
        }
        if execution.answer == Answer::CommitFails {
            report(out, b'E', "ERROR", "40001", "could not serialize access");
        }
        message(out, b'Z', b"I");
        self.finish(RoundOf::Query(k, j), execution.cut, start);
        Ok(())
    }

    /// Records where `round`'s answer, from `start`, ends; or cuts it there and stops.
    fn finish(&mut self, round: RoundOf, cut: Cut, start: usize) {
        let end = self.out.len();
        match cut {
            Cut::CloseAt(at) => {
                let at = start + at % (end - start);
                self.out.truncate(at);
                self.close_at = Some(at);
                self.stopped = true;
            }
            Cut::Whole | Cut::Silent => self.ends.push((round, end)),
        }
    }

    fn end_of(&self, round: RoundOf) -> Option<usize> {
        self.ends.iter().find(|(it, _)| *it == round).map(|(_, end)| *end)
    }
}

/// Checks a `Bind`'s formats and values (after its portal and statement) against the plan.
fn check_bind(reader: &mut Body<'_>, params: &[ParamType], values: &[Value]) -> Result<(), String> {
    if (reader.u16()?, reader.u16()?) != (1, 1) {
        return Err("parameters not all binary".to_string());
    }
    if usize::from(reader.u16()?) != params.len() {
        return Err("another number of parameters".to_string());
    }
    for (index, (param, value)) in params.iter().zip(values).enumerate() {
        let len = reader.i32()?;
        let body = if len < 0 { None } else { Some(reader.take(len as usize)?) };
        let decoded = match (*param, value, body) {
            (_, Value::Of(Owned::Null), None) => continue,
            (ParamType::Scalar(storage), Value::Of(want), Some(body)) => {
                codec::decode(storage, Some(body)).map(|got| Owned::of(got) == *want)
            }
            (ParamType::Array(storage), Value::Array(items), Some(body)) => {
                codec::decode_array(storage, body).map(|mut elements| {
                    let got: Vec<Option<Owned>> =
                        elements.by_ref().map(|it| it.ok().map(Owned::of)).collect();
                    elements.finish().is_ok()
                        && got.len() == items.len()
                        && got.iter().zip(items).all(|(got, want)| got.as_ref() == Some(want))
                })
            }
            _ => Ok(false),
        };
        if decoded != Ok(true) {
            return Err(format!("parameter {index} is not {value:?}: {decoded:?}"));
        }
    }
    if (reader.u16()?, reader.u16()?) != (1, 1) {
        return Err("results not all binary".to_string());
    }
    reader.end()
}

// ---------------------------------------------------------------------------------------------
// What the connection said.

/// How a round ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum End {
    Prepared,
    Done(Option<u64>),
    /// Failed with the server's error of this code.
    Server(String),
    /// Failed because the description differs: the error's name.
    Mismatch(&'static str),
    Deadline,
    Closed,
    Other(String),
}

fn end_of(error: &Error<'_>) -> End {
    match error {
        Error::Server(error) => End::Server(error.code.to_string()),
        Error::ParamType { .. } => End::Mismatch("ParamType"),
        Error::ParamCount { .. } => End::Mismatch("ParamCount"),
        Error::ColumnType { .. } => End::Mismatch("ColumnType"),
        Error::ColumnName { .. } => End::Mismatch("ColumnName"),
        Error::ColumnCount { .. } => End::Mismatch("ColumnCount"),
        Error::Returns { .. } => End::Mismatch("Returns"),
        Error::Deadline { .. } => End::Deadline,
        Error::Closed(_) => End::Closed,
        other => End::Other(other.to_string()),
    }
}

/// How the round ends when the server's whole answer reaches the connection, and how many of
/// its rows come first. A failure is said at the server's error, so a cut answer can still end
/// this way.
fn planned_end(plan: &Plan, round: RoundOf) -> Option<(End, usize)> {
    match round {
        RoundOf::Prepare(k) => {
            let planned = plan.statements.get(k)?;
            Some(match (planned.describe, planned.describe.mismatch()) {
                (_, Some(name)) => (End::Mismatch(name), 0),
                (Describe::Error, None) => (End::Server("42P01".to_string()), 0),
                _ => (End::Prepared, 0),
            })
        }
        RoundOf::Query(k, j) => {
            let planned = plan.statements.get(k)?;
            let execution = planned.executions.get(j)?;
            let all = execution.rows.len();
            let changed = match planned.columns {
                Some(_) => all as u64,
                None => u64::from(execution.count),
            };
            Some(match execution.answer {
                Answer::Rows => (End::Done(Some(changed)), all),
                Answer::ErrorAt(at) => (End::Server("P0001".to_string()), at.min(all)),
                Answer::CommitFails => (End::Server("40001".to_string()), all),
            })
        }
    }
}

/// The rows a round can have.
fn planned_rows(plan: &Plan, round: RoundOf) -> &[Vec<Owned>] {
    match round {
        RoundOf::Prepare(_) => &[],
        RoundOf::Query(k, j) => plan
            .statements
            .get(k)
            .and_then(|planned| planned.executions.get(j))
            .map_or(&[][..], |execution| &execution.rows[..]),
    }
}

struct Issued {
    round: RoundOf,
    deadline: u64,
    rows: Vec<Vec<Owned>>,
    end: Option<End>,
}

/// What the harness knows of one statement.
#[derive(Clone, Copy, Default)]
struct Asked {
    prepared: bool,
    /// From its prepare's event: whether it was prepared.
    outcome: Option<bool>,
    queries: usize,
    closed: bool,
    /// A query on it was tried while its prepare was in flight, and after it failed.
    probed_preparing: bool,
    probed_failed: bool,
}

struct Seen {
    ready: bool,
    issued: Vec<Issued>,
    /// The close's reason and how much of the server's stream had been delivered then.
    closed: Option<(String, usize)>,
}

/// One run of the plan; the draws pick its steps.
struct Run<'p> {
    tape: &'p Tape,
    plan: &'p Plan,
    peer: Peer<'p>,
    connection: Connection<Global>,
    now: u64,
    /// How much of the server's stream reached the connection.
    delivered: usize,
    told_closed: bool,
    asked: Vec<Asked>,
    seen: Seen,
}

/// What the harness can ask next.
#[derive(Clone, Copy, Debug)]
enum Ask {
    Prepare(usize),
    Query(usize),
    /// A query on a statement that must be refused.
    Probe(usize),
    Close(usize),
}

impl<'p> Run<'p> {
    fn issued(&mut self, round: RoundOf) -> Result<&mut Issued, Broke> {
        match self.seen.issued.iter_mut().find(|it| it.round == round) {
            Some(issued) => Ok(issued),
            None => oracle!("an event of {round:?}, which was not asked"),
        }
    }

    fn end(&mut self, round: RoundOf, end: End) -> Result<(), Broke> {
        let issued = self.issued(round)?;
        if let Some(first) = &issued.end {
            oracle!("{round:?} ended twice: {first:?}, then {end:?}");
        }
        issued.end = Some(end);
        Ok(())
    }

    fn query_round(&self, id: QueryId) -> Result<RoundOf, Broke> {
        let k = (id.0 / 16) as usize;
        let j = (id.0 % 16) as usize;
        if k >= self.plan.statements.len() {
            oracle!("an event of {id}, which was not asked");
        }
        Ok(RoundOf::Query(k, j))
    }

    fn drain(&mut self) -> Result<(), Broke> {
        // Bounded by the events the calls before queued.
        loop {
            let delivered = self.delivered;
            let ready = self.seen.ready;
            let closed = self.seen.closed.clone();
            let Some(event) = self.connection.next_event() else { break };
            if let Some((why, _)) = &closed {
                oracle!("{event:?} after the close ({why})");
            }
            // What the event says, owned, so the connection's borrow ends here.
            enum Said {
                Ready,
                Prepared(StatementId, End),
                Row(QueryId, Vec<Owned>),
                Ended(QueryId, End),
                Notice,
                Close(String),
            }
            let said = match event {
                Event::Ready => Said::Ready,
                Event::Prepared { statement } => Said::Prepared(statement, End::Prepared),
                Event::PrepareFailed { statement, error } => {
                    Said::Prepared(statement, end_of(&error))
                }
                Event::Row { id, columns } => {
                    let mut values = Vec::with_capacity(columns.len());
                    for value in columns.iter() {
                        match value {
                            Ok(value) => values.push(Owned::of(value)),
                            Err(error) => oracle!("a value of {id}'s row does not decode: {error}"),
                        }
                    }
                    Said::Row(id, values)
                }
                Event::Done { id, changed } => Said::Ended(id, End::Done(changed)),
                Event::Failed { id, error } => Said::Ended(id, end_of(&error)),
                Event::Notice(_) => Said::Notice,
                Event::Close { why } => {
                    let allowed = match why {
                        Fatal::Deadline { .. } | Fatal::PeerClosed { .. } => true,
                        Fatal::StartupDeadline { .. } => !ready,
                        _ => false,
                    };
                    if !allowed {
                        oracle!("the connection closed with an honest server: {why}");
                    }
                    Said::Close(why.to_string())
                }
            };
            match said {
                Said::Ready => {
                    if ready {
                        oracle!("Ready twice");
                    }
                    self.seen.ready = true;
                }
                Said::Prepared(statement, end) => {
                    let k = statement.0 as usize;
                    let Some(asked) = self.asked.get_mut(k) else {
                        oracle!("an outcome of {statement}, which was not prepared");
                    };
                    asked.outcome = Some(end == End::Prepared);
                    self.end(RoundOf::Prepare(k), end)?;
                }
                Said::Row(id, row) => {
                    let round = self.query_round(id)?;
                    let issued = self.issued(round)?;
                    if issued.end.is_some() {
                        oracle!("a row of {id} after it ended");
                    }
                    issued.rows.push(row);
                }
                Said::Ended(id, end) => {
                    let round = self.query_round(id)?;
                    self.end(round, end)?;
                }
                Said::Notice => {}
                Said::Close(why) => self.seen.closed = Some((why, delivered)),
            }
        }
        if self.seen.closed.is_some()
            && (self.connection.next_send().is_some() || self.connection.next_deadline().is_some())
        {
            oracle!("the connection still sends or waits after its close");
        }
        Ok(())
    }

    /// Every round whose deadline the clock reached has ended.
    fn deadlines_kept(&self) -> Result<(), Broke> {
        for issued in &self.seen.issued {
            if issued.end.is_none() && issued.deadline <= self.now {
                oracle!(
                    "{:?} outlived its deadline {} (now {})",
                    issued.round,
                    issued.deadline,
                    self.now
                );
            }
        }
        Ok(())
    }

    fn ack(&mut self, all: bool) -> Result<bool, Broke> {
        let Some(bytes) = self.connection.next_send() else { return Ok(false) };
        let len = bytes.len();
        let n = if all {
            len
        } else {
            self.tape.int_in(1..=len, "socket takes")
        };
        let chunk = bytes.get(..n).unwrap_or(bytes).to_vec();
        self.connection.sent(n);
        self.peer.take(&chunk)?;
        self.drain()?;
        Ok(true)
    }

    fn deliver(&mut self, all: bool) -> Result<bool, Broke> {
        let avail = self.peer.out.len() - self.delivered;
        let mut moved = false;
        if avail > 0 {
            let n = if all {
                avail
            } else {
                self.tape.int_in(1..=avail, "delivered")
            };
            let chunk = &self.peer.out[self.delivered..self.delivered + n];
            self.connection.received(chunk);
            self.delivered += n;
            moved = true;
        }
        if !self.told_closed && self.peer.close_at == Some(self.delivered) {
            self.told_closed = true;
            self.connection.closed();
            moved = true;
        }
        self.drain()?;
        Ok(moved)
    }

    fn tick(&mut self, to: u64) -> Result<(), Broke> {
        self.now = self.now.max(to);
        self.connection.advance_clock(self.now);
        self.drain()?;
        self.deadlines_kept()
    }

    /// What can be asked now.
    fn asks(&self) -> Vec<Ask> {
        let mut asks = Vec::new();
        if !self.seen.ready || self.seen.closed.is_some() {
            return asks;
        }
        // Statements are prepared in order.
        if let Some(k) = self.asked.iter().position(|it| !it.prepared) {
            asks.push(Ask::Prepare(k));
        }
        for (k, asked) in self.asked.iter().enumerate() {
            let planned = &self.plan.statements[k];
            if !asked.prepared || asked.closed {
                continue;
            }
            match asked.outcome {
                None if !asked.probed_preparing => asks.push(Ask::Probe(k)),
                Some(false) if !asked.probed_failed => asks.push(Ask::Probe(k)),
                Some(true) if asked.queries < planned.executions.len() => asks.push(Ask::Query(k)),
                Some(ok) if planned.close && (!ok || asked.queries == planned.executions.len()) => {
                    asks.push(Ask::Close(k));
                }
                _ => {}
            }
        }
        asks
    }

    /// Asks one of `asks`, by `pick`.
    fn ask(&mut self, ask: Ask) -> Result<(), Broke> {
        let plan: &'p Plan = self.plan;
        match ask {
            Ask::Prepare(k) => {
                let planned = &plan.statements[k];
                let deadline = self.now + self.tape.int_in(1..=5_000u64, "prepare deadline");
                let columns: Vec<(&str, Storage)> = planned
                    .columns
                    .iter()
                    .flatten()
                    .map(|(name, storage)| (name.as_str(), *storage))
                    .collect();
                let returns = match planned.columns {
                    Some(_) => Expect::Rows(&columns),
                    None => Expect::Count,
                };
                let refused = self.connection.prepare(
                    statement_id(k),
                    &planned.text,
                    &planned.params,
                    returns,
                    deadline,
                );
                self.asked[k].prepared = true;
                match refused {
                    Ok(()) => self.seen.issued.push(Issued {
                        round: RoundOf::Prepare(k),
                        deadline,
                        rows: Vec::new(),
                        end: None,
                    }),
                    Err(refused) => oracle!("the prepare of statement {k} was refused: {refused}"),
                }
            }
            Ask::Query(k) => {
                let planned = &plan.statements[k];
                let j = self.asked[k].queries;
                let execution = &planned.executions[j];
                let deadline = self.now + self.tape.int_in(1..=5_000u64, "query deadline");
                let arrays: Vec<Vec<ValueRef<'_>>> = execution
                    .values
                    .iter()
                    .map(|value| match value {
                        Value::Array(items) => items.iter().map(Owned::as_ref).collect(),
                        Value::Of(_) => Vec::new(),
                    })
                    .collect();
                let values: Vec<Bound<'_>> = execution
                    .values
                    .iter()
                    .zip(&arrays)
                    .map(|(value, items)| match value {
                        Value::Of(value) => Bound::Value(value.as_ref()),
                        Value::Array(_) => Bound::Array(items),
                    })
                    .collect();
                let id = query_id(k, j);
                let sent = self.connection.query(id, statement_id(k), &values, deadline);
                self.asked[k].queries += 1;
                match sent {
                    Ok(()) => {
                        self.seen.issued.push(Issued {
                            round: RoundOf::Query(k, j),
                            deadline,
                            rows: Vec::new(),
                            end: None,
                        });
                    }
                    Err(refused) => oracle!("{id} on a prepared statement was refused: {refused}"),
                }
            }
            Ask::Probe(k) => {
                let statement = statement_id(k);
                let asked = self.asked[k];
                let want = match asked.outcome {
                    None => Refused::StatementPreparing { statement },
                    _ => Refused::StatementFailed { statement },
                };
                if asked.outcome.is_none() {
                    self.asked[k].probed_preparing = true;
                } else {
                    self.asked[k].probed_failed = true;
                }
                let got = self.connection.query(QueryId(u64::MAX), statement, &[], self.now + 1);
                if got != Err(want) {
                    oracle!(
                        "a query on statement {k} ({:?}) gave {got:?}, not {want:?}",
                        asked.outcome
                    );
                }
            }
            Ask::Close(k) => {
                self.asked[k].closed = true;
                if let Err(refused) = self.connection.close_statement(statement_id(k)) {
                    oracle!("the close of statement {k} was refused: {refused}");
                }
            }
        }
        self.drain()
    }

    /// Asks one of what can be asked, drawn; false when nothing can be.
    fn ask_drawn(&mut self) -> Result<bool, Broke> {
        let asks = self.asks();
        if asks.is_empty() {
            return Ok(false);
        }
        let pick = self.tape.index(asks.len(), "ask");
        self.ask(asks[pick])?;
        Ok(true)
    }
}

/// One run: the drawn steps, then everything asked, sent and delivered, then the clock past
/// every deadline; then the ends are held to the plan.
fn run(tape: &Tape, plant: Option<Plant>) -> Result<(), Broke> {
    let plan = tape.span("plan", draw_plan);
    let config = Config {
        user: USER,
        database: DATABASE,
        password: PASSWORD,
        startup_deadline: plan.startup_deadline,
    };
    let nonce: [u8; 18] = core::array::from_fn(|at| (at as u8).wrapping_mul(37));
    let mut connection = Connection::new(config, nonce, Global);
    if let Some(plant) = plant {
        connection.plant(plant);
    }
    let mut run = Run {
        tape,
        plan: &plan,
        peer: Peer::new(&plan),
        connection,
        now: 0,
        delivered: 0,
        told_closed: false,
        asked: plan.statements.iter().map(|_| Asked::default()).collect(),
        seen: Seen { ready: false, issued: Vec::new(), closed: None },
    };
    run.drain()?;

    let steps = tape.int_in(0..=STEPS_MOST, "steps");
    for _ in 0..steps {
        match tape.int_in(0..=3u8, "step") {
            0 => {
                run.ack(false)?;
            }
            1 => {
                run.deliver(false)?;
            }
            2 => {
                let step = tape.int_in(0..=400u64, "clock step");
                run.tick(run.now + step)?;
            }
            _ => {
                run.ask_drawn()?;
            }
        }
    }

    // Finish: ask the rest, send and deliver everything until nothing moves.
    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > ROUNDS_MOST {
            harness!("the exchange did not settle in {ROUNDS_MOST} rounds");
        }
        let asked = match run.asks().first() {
            Some(&ask) => {
                run.ask(ask)?;
                true
            }
            None => false,
        };
        let acked = run.ack(true)?;
        let delivered = run.deliver(true)?;
        if !(asked || acked || delivered) {
            break;
        }
    }
    let last =
        run.seen.issued.iter().map(|it| it.deadline).max().unwrap_or(0).max(plan.startup_deadline);
    run.tick(last)?;
    check_ends(&run)
}

/// The ends against the plan, once the run is over.
fn check_ends(run: &Run<'_>) -> Result<(), Broke> {
    let seen = &run.seen;
    // What had reached the connection when it closed: an answer within it was read whole.
    let read = seen.closed.as_ref().map_or(run.delivered, |(_, delivered)| *delivered);
    for issued in &seen.issued {
        let round = issued.round;
        let Some(end) = &issued.end else {
            oracle!("{round:?} never ended");
        };
        let own = planned_rows(run.plan, round);
        let whole = run.peer.end_of(round).is_some_and(|at| at <= read);
        let expected = planned_end(run.plan, round);
        match (whole, &expected) {
            (true, Some((want, rows))) => {
                let rows = (*rows).min(own.len());
                if end != want || issued.rows[..] != own[..rows] {
                    oracle!(
                        "{round:?} was answered whole: it ended {end:?} with {} rows {:?}, and the plan \
                         says {want:?} with its first {rows} rows",
                        issued.rows.len(),
                        issued.rows
                    );
                }
            }
            _ => {
                // Not answered whole: it failed, by the close, its deadline or its own answer's
                // failure, with only a prefix of its own rows.
                let own_failure = expected.as_ref().is_some_and(|(want, _)| {
                    want == end && !matches!(want, End::Done(_) | End::Prepared)
                });
                if !(matches!(end, End::Deadline | End::Closed) || own_failure) {
                    oracle!("{round:?} was not answered whole and ended {end:?}");
                }
                let prefix =
                    issued.rows.len() <= own.len() && issued.rows[..] == own[..issued.rows.len()];
                if !prefix {
                    oracle!("{round:?} got rows {:?}, not a prefix of its own", issued.rows);
                }
            }
        }
    }
    Ok(())
}

/// The invariant: every prepare and query the connection took ends in exactly one outcome
/// (`Prepared` or `PrepareFailed`; `Done` or `Failed`); a round whose whole answer arrived ends
/// as the server answered it, a query with exactly its own rows in order (so after a failed
/// round the next one's rows are its own); a round not answered whole fails, with only a prefix
/// of its own rows; a statement whose description differs is never executed: the scripted
/// server sees no `Bind` for it, and a query on it, or on one still preparing, is refused by
/// name; nothing is said after `Close`, which an honest server causes only by closing or by a
/// deadline passing; and no round outlives its deadline. `BLAZE_SIM_SEEDS` runs it over more
/// seeds.
#[test]
fn every_round_ends_once_with_its_own_rows() {
    blaze_sim::check(blaze_sim::Config::cases(300), |tape| run(tape, None));
}

/// The planted-bug control: a core that ends a failed round at its error, instead of skipping
/// what the server sends for it up to its `ReadyForQuery`, reads the rest of that answer as the
/// next round's. The oracle must see it, and the same cases with the honest core must pass.
#[test]
fn the_oracle_catches_a_core_that_does_not_skip_to_ready() {
    blaze_sim::control(
        blaze_sim::Config::cases(600),
        None,
        Some(Plant::NoSkipToReady),
        "",
        run,
    );
}
