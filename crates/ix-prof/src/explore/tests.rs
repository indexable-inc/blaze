//! The session core on a clock of the tests' own; the server's machine ([`super::serving`])
//! under blaze-core's deterministic simulation, with its HTTP/3 driver loop and a page's
//! client on one [`Sim`]; and the server end to end on a real socket over a test source: the
//! CONNECT rules, hello first, the token, an answer, an unknown op, a late request, the live
//! stream and the stop, as the page sees them.

use std::{
    collections::{BTreeMap, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arrow_array::{Int64Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use arrow_schema::{DataType, Field, Schema};
use base64::Engine as _;
use blaze_core::{
    Driver as _, Effect, Tag,
    sim::{Datagrams, Faults, Sim},
};
use blaze_http::{
    h3::{
        ClientConfig, Closed, Direction, Endpoint, Event, ServerConfig, SessionId, StreamRef,
        Transport, Trust,
        driver::{self as h3_driver, Driver, Loop, Session as Connection},
    },
    mailbox,
};
use blaze_sim::{Broke, Tape};
use serde_json::Value;

use super::{
    Answer, Config, Explorer, Hello, Kind, Layer, Live, OpError, PROTOCOL, Rule, Source, Stop,
    Table, Tick,
    cert::{Cert, Rotation},
    frame, link,
    link::{Origins, Token},
    serve,
    server::work_on,
    serving::{self, Clock, Done, JobId, Plant, Serving, Setup},
    session::{
        self, CLOSE_BAD_TOKEN, CLOSE_STOPPED, Emit, Feed, MAX_IN_FLIGHT, Phase, REQUEST_WAIT,
        Served, Session, Step,
    },
};

const LAYERS: &[Layer] = &[Layer { name: "test", version: 3 }];

fn hello() -> Hello {
    Hello {
        daemon: "test 0.1".to_owned(),
        schema: 3,
        clock: crate::clock::CLOCK,
        now_ns: 42,
        kind: "test",
        extensions: &["test"],
        layers: LAYERS,
    }
}

fn numbers(n: i64) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
    RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from_iter_values(0..n))]).unwrap()
}

fn decode(ipc: &[u8]) -> Vec<RecordBatch> {
    StreamReader::try_new(ipc, None).unwrap().collect::<Result<_, _>>().unwrap()
}

/// A source with one op, `count` (`n` rows), and a live feed of a `spans` table of two rows a
/// tick and an `open` table of one row on its first tick only.
struct Test;

impl Source for Test {
    fn hello(&self) -> Hello {
        Hello { now_ns: crate::clock::now_ns(), ..hello() }
    }

    fn live(&self) -> Box<dyn Live> {
        Box::new(TestLive { ticks: 0 })
    }

    fn op(&self, name: &str, args: &Value, _stop: &Stop) -> Option<Result<Answer, OpError>> {
        match name {
            "count" => Some(match args.get("n").and_then(Value::as_i64) {
                Some(n) => Ok(Answer { table: Table::of(vec![numbers(n)]), ..Answer::new("count") }
                    .with("n", n)),
                None => Err(OpError::new(Kind::BadArgs, "`count` takes a whole `n`")),
            }),
            _ => None,
        }
    }
}

struct TestLive {
    ticks: u64,
}

impl Live for TestLive {
    fn tick(&mut self, _at_ns: i64, into: &mut Tick) -> Result<(), OpError> {
        self.ticks += 1;
        let spans = into.table("spans", Rule::Append);
        spans.rows = 2;
        frame::ipc(&mut spans.ipc, &numbers(2).schema(), &[numbers(2)])
            .map_err(|why| OpError::new(Kind::Encode, why.to_string()))?;
        let open = into.table("open", Rule::Replace);
        if self.ticks == 1 {
            open.rows = 1;
            frame::ipc(&mut open.ipc, &numbers(1).schema(), &[numbers(1)])
                .map_err(|why| OpError::new(Kind::Encode, why.to_string()))?;
        }
        Ok(())
    }
}

const TOKEN: [u8; 16] = [7; 16];
const GOOD: &[u8] = br#"{"op":"hello","token":"07070707070707070707070707070707"}"#;
const WRONG: &[u8] = br#"{"op":"hello","token":"07070707070707070707070707070708"}"#;

fn refused(step: Step) -> OpError {
    match step {
        Step::Refuse(error) => error,
        other => panic!("refused, not {other:?}"),
    }
}

#[test]
fn hello_comes_first_then_the_source_answers() {
    let token = Token::from_bytes(TOKEN);
    let mut core = Session::new("test explore");
    for before in [&br#"{"op":"count","n":1}"#[..], br#"{"op":"frobnicate"}"#] {
        let error = refused(core.request(&token, before));
        assert_eq!(error.kind, "unauthenticated", "{}: {error}", String::from_utf8_lossy(before));
    }
    for bad in [
        &br#"not json"#[..],
        br#"{"op":7}"#,
        br#"[1]"#,
        br#"{"op":"hello"}"#,
        br#"{"op":"hello","token":5}"#,
        br#"{"op":"hello","token":"07070707070707070707070707070707","extra":1}"#,
    ] {
        let error = refused(core.request(&token, bad));
        assert_eq!(error.kind, "bad_args", "{}: {error}", String::from_utf8_lossy(bad));
    }
    assert_eq!(core.phase(), Phase::Hello, "nothing before a good hello opens the session");
    assert_eq!(core.request(&token, GOOD), Step::Hello { first: true });
    assert_eq!(core.phase(), Phase::Open);
    assert_eq!(
        core.request(&token, br#"{"op":"count","n":3,"deep":{"op":"no"}}"#),
        Step::Op { name: "count".to_owned() },
        "the source's op, which the driver gives the request whole"
    );
    for (bad, says) in [
        (&br#"{"op":"hello","token":5}"#[..], "`token` is a number, not a string"),
        (br#"{"op":"hello","token":{"a":[1]}}"#, "`token` is an object, not a string"),
        (br#"{"op":"hello"}"#, "missing field `token`"),
        (
            br#"{"op":"hello","token":"07070707070707070707070707070707","extra":[1]}"#,
            "unknown field `extra`",
        ),
        (br#"[{"op":"hello"}]"#, "no string `op`"),
        (br#""hello""#, "no string `op`"),
    ] {
        let error = refused(core.request(&token, bad));
        assert_eq!(error.kind, "bad_args", "{}: {error}", String::from_utf8_lossy(bad));
        assert!(error.message.contains(says), "{}: {error}", String::from_utf8_lossy(bad));
    }
    assert_eq!(core.request(&token, GOOD), Step::Hello { first: false }, "hello again");
}

#[test]
fn a_wrong_token_closes_the_session_before_or_after_hello() {
    let token = Token::from_bytes(TOKEN);
    for after_hello in [false, true] {
        let mut core = Session::new("blaze explore");
        if after_hello {
            assert_eq!(core.request(&token, GOOD), Step::Hello { first: true });
        }
        let Step::Close { error, code } = core.request(&token, WRONG) else {
            panic!("a wrong token closes the session");
        };
        assert_eq!((error.kind, code), ("bad_token", CLOSE_BAD_TOKEN));
        assert!(error.message.contains("`blaze explore`"), "it names the command: {error}");
        assert_eq!(core.phase(), Phase::Closed);
        for after in [GOOD, br#"{"op":"count","n":1}"#] {
            let error = refused(core.request(&token, after));
            assert_eq!(error.kind, "unauthenticated", "a closed session answers nothing");
        }
    }
}

/// On a clock of the test's own: sixteen in flight, the seventeenth refused naming the limit,
/// and a slot given back makes room; each slot's `elapsed_ns` is from its own opening.
#[test]
fn at_most_sixteen_requests_are_in_flight() {
    let mut core = Session::new("test explore");
    let mut slots: Vec<_> =
        (0..MAX_IN_FLIGHT as i64).map(|i| core.opened(i * 10).unwrap()).collect();
    assert_eq!(core.in_flight(), MAX_IN_FLIGHT);
    let over = core.opened(1_000).unwrap_err();
    assert_eq!(over.kind, "overloaded");
    assert_eq!(over.detail["in_flight"], MAX_IN_FLIGHT);
    assert_eq!(core.in_flight(), MAX_IN_FLIGHT, "a refused stream holds no slot");
    let second = slots.remove(1);
    assert_eq!(second.started_ns(), 10);
    assert_eq!(second.elapsed_ns(1_010), 1_000);
    assert_eq!(second.elapsed_ns(0), 0, "a reading before the opening is no time");
    core.ended(second);
    assert_eq!(core.in_flight(), MAX_IN_FLIGHT - 1);
    let again = core.opened(2_000).unwrap();
    assert_eq!(again.started_ns(), 2_000);
    assert!(core.opened(2_001).is_err(), "full again");
    for slot in slots {
        core.ended(slot);
    }
    core.ended(again);
    assert_eq!(core.in_flight(), 0);
}

#[test]
fn an_emptied_replace_set_is_sent_once_as_no_rows() {
    let mut feed = Feed::default();
    assert_eq!(feed.emit("open", Rule::Replace, 0), Emit::Nothing, "an idle source sends nothing");
    assert_eq!(feed.emit("open", Rule::Replace, 3), Emit::Rows);
    assert_eq!(feed.emit("open", Rule::Replace, 3), Emit::Rows);
    assert_eq!(feed.emit("open", Rule::Replace, 0), Emit::Cleared, "the page clears what it shows");
    assert_eq!(feed.emit("open", Rule::Replace, 0), Emit::Nothing, "once");
    assert_eq!(feed.emit("tasks", Rule::Replace, 2), Emit::Rows);
    assert_eq!(feed.emit("open", Rule::Replace, 0), Emit::Nothing, "each table on its own");
    assert_eq!(feed.emit("tasks", Rule::Replace, 0), Emit::Cleared);
    for table in ["spans", "counters"] {
        assert_eq!(feed.emit(table, Rule::Append, 5), Emit::Rows);
        let emit = feed.emit(table, Rule::Append, 0);
        assert_eq!(emit, Emit::Nothing, "{table} appends: nothing to clear");
    }
}

#[test]
fn hello_says_the_source_the_certificate_the_protocol_and_the_layers() {
    let served = Served { hash: "h".to_owned(), not_after_s: 99 };
    let source = hello();
    let header = session::hello_header(&source, &served, Duration::from_millis(250), None);
    let text = serde_json::to_string(&header).unwrap();
    assert_eq!(
        text,
        format!(
            r#"{{"op":"hello","daemon":"test 0.1","schema":3,"clock":"{}","now_ns":42,"certhash":"h","cert_not_after_unix_s":99,"live_every_ms":250,"source":{{"kind":"test","extensions":["test"]}},"protocol":{PROTOCOL},"layers":[{{"name":"test","version":3}}]}}"#,
            crate::clock::CLOCK
        ),
        "the header's bytes, its fields in this order whatever the build turns on in serde_json"
    );
    let header: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(header["op"], "hello");
    assert_eq!(header["daemon"], "test 0.1");
    assert_eq!(header["schema"], 3);
    assert_eq!(header["clock"], crate::clock::CLOCK);
    assert_eq!(header["now_ns"], 42);
    assert_eq!(header["certhash"], "h");
    assert_eq!(header["cert_not_after_unix_s"], 99);
    assert_eq!(header["live_every_ms"], 250);
    assert_eq!(header["source"], serde_json::json!({"kind": "test", "extensions": ["test"]}));
    assert_eq!(header["protocol"], PROTOCOL);
    assert_eq!(header["protocol"], 1, "the first protocol");
    assert_eq!(header["layers"], serde_json::json!([{"name": "test", "version": 3}]));
    assert_eq!(header.as_object().unwrap().len(), 11, "{header}");
}

/// A wasm module holding only the core's ABI section, `abi`.
fn core_module(abi: u32) -> Vec<u8> {
    let name = super::wasm::SECTION.as_bytes();
    let mut out = vec![0, b'a', b's', b'm', 1, 0, 0, 0, 0];
    out.push(u8::try_from(1 + name.len() + 4).unwrap());
    out.push(u8::try_from(name.len()).unwrap());
    out.extend_from_slice(name);
    out.extend_from_slice(&abi.to_le_bytes());
    out
}

#[test]
fn hello_names_the_core_served_and_the_core_op_answers_its_bytes_after_an_aligned_header() {
    let bytes = core_module(1);
    let core = super::CoreWasm::new(bytes.clone(), "core.wasm").unwrap();
    let served = Served { hash: "h".to_owned(), not_after_s: 99 };
    let source = hello();
    let header = session::hello_header(&source, &served, Duration::from_millis(250), Some(&core));
    let header = serde_json::to_value(header).unwrap();
    let said = serde_json::json!({"abi": 1, "sha256": core.sha256, "bytes": bytes.len()});
    assert_eq!(header["core"], said);
    assert_eq!(header.as_object().unwrap().len(), 12, "{header}");

    let answer = session::core_bytes(&Ok(core.clone()));
    let len = u32::from_le_bytes(answer[..4].try_into().unwrap()) as usize;
    assert_eq!((4 + len) % frame::ALIGN, 0, "the module starts 8-aligned");
    let header: Value = serde_json::from_slice(&answer[4..4 + len]).unwrap();
    assert_eq!(header["op"], "core");
    assert_eq!(header["sha256"], core.sha256.as_str());
    assert_eq!(&answer[4 + len..], &bytes[..], "the raw module follows its header");

    let none = session::core_bytes(&Err("no core at /x/explore-core.wasm".to_owned()));
    let header: Value = serde_json::from_slice(&none[4..]).unwrap();
    assert_eq!(header["error"]["kind"], "no_core");
    assert_eq!(header["error"]["message"], "no core at /x/explore-core.wasm");

    let token = Token::random();
    let mut session = Session::new("test explore");
    let hello = format!(r#"{{"op":"hello","token":"{}"}}"#, token.as_str());
    assert_eq!(session.request(&token, hello.as_bytes()), Step::Hello { first: true });
    assert_eq!(session.request(&token, br#"{"op":"core"}"#), Step::Core);
}

#[test]
fn an_unknown_op_is_named_with_the_layers_that_have_ops() {
    let error = session::unknown_op("frobnicate", &hello());
    assert_eq!(error.kind, "unknown_op");
    assert_eq!(error.detail["op"], "frobnicate");
    assert!(error.message.contains("`frobnicate`") && error.message.contains("test 3"), "{error}");
}

#[test]
fn an_answer_carries_rows_and_elapsed_and_one_aligned_ipc_stream() {
    let answer = Answer { table: Table::of(vec![numbers(5), numbers(2)]), ..Answer::new("count") };
    let bytes = frame::answer_bytes(Ok(answer), 1234);
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!((4 + len) % frame::ALIGN, 0, "the IPC starts 8-aligned");
    let header: Value = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
    assert_eq!((header["op"].as_str(), header["rows"].as_u64()), (Some("count"), Some(7)));
    assert_eq!(header["elapsed_ns"], 1234);
    let rows: usize = decode(&bytes[4 + len..]).iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows, 7);
}

#[test]
fn an_answer_that_fails_becomes_its_error_with_no_ipc() {
    let bytes = frame::answer_bytes(Err(OpError::new("sql", "no")), 0);
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(bytes.len(), 4 + len, "an error has no IPC");
    let header: Value = serde_json::from_slice(&bytes[4..]).unwrap();
    assert_eq!(header["error"]["kind"], "sql");
    assert_eq!(header["error"]["message"], "no");
}

#[test]
fn an_error_header_has_its_kind_message_and_detail() {
    let failed = OpError::new("sql", "no such table").with("query", "select * from x");
    let text = serde_json::to_string(&failed.header()).unwrap();
    assert_eq!(
        text,
        r#"{"error":{"kind":"sql","message":"no such table","query":"select * from x"}}"#
    );
    let live = serde_json::to_string(&session::live_error_header(&failed)).unwrap();
    assert_eq!(
        live,
        r#"{"error":{"kind":"sql","message":"no such table","query":"select * from x"},"op":"live"}"#
    );
    // A detail set twice keeps its place and its last value; one named as the error's own
    // fields is the one written.
    let twice = OpError::new("sql", "no").with("a", 1).with("b", 2).with("a", 3).with("kind", "x");
    assert_eq!(
        serde_json::to_string(&twice.header()).unwrap(),
        r#"{"error":{"message":"no","a":3,"b":2,"kind":"x"}}"#
    );
}

#[test]
fn the_live_and_cert_headers_are_their_fields_in_order() {
    let tick = Tick { at_ns: 7, skipped: 2, lost: 1, ..Tick::default() };
    assert_eq!(
        serde_json::to_string(&session::live_header("spans", 3, &tick)).unwrap(),
        r#"{"op":"live","table":"spans","rows":3,"at_ns":7,"skipped":2,"lost":1}"#
    );
    let served = Served { hash: "h".to_owned(), not_after_s: 99 };
    assert_eq!(
        serde_json::to_string(&session::cert_header(&served)).unwrap(),
        r#"{"op":"cert","certhash":"h","not_after_unix_s":99}"#
    );
}

fn explorer(dev_origin: bool) -> Explorer {
    serve(Config {
        source: Arc::new(Test),
        dev_origin,
        name: "test explore",
        every: Duration::from_millis(250),
        core: Err("the test serves no core".to_owned()),
    })
    .unwrap()
}

/// A client pinning the explorer's certificate, as the page does, on a driver thread of its
/// own; its handles' futures are polled by the runtime the test awaits them on.
fn client(explorer: &Explorer) -> Driver {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(explorer.certhash()).unwrap();
    let config =
        ClientConfig { trust: Trust::Pinned(vec![raw.try_into().unwrap()]), transport: Transport::default() };
    Driver::spawn(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), None, Some(config)).unwrap().0
}

/// How long a real-socket test waits for any one thing: loopback answers in milliseconds.
const WITHIN: Duration = Duration::from_secs(10);

/// `future` polled on this thread to its end within [`WITHIN`], waiting for `what`.
fn wait<F: std::future::Future>(what: &'static str, future: F) -> F::Output {
    match mailbox::block_on(what, Some(WITHIN), future) {
        Ok(it) => it,
        Err(why) => panic!("{why}"),
    }
}

fn connect(
    client: &Driver,
    explorer: &Explorer,
    path: &str,
    origin: Option<&str>,
) -> Result<Connection, Closed> {
    let url = format!("https://127.0.0.1:{}{path}", explorer.port());
    let to = SocketAddr::from((Ipv4Addr::LOCALHOST, explorer.port()));
    let headers: Vec<(&str, &str)> = origin.map(|it| ("origin", it)).into_iter().collect();
    wait("the explorer's answer to a CONNECT", client.connect(to, &url, &headers))
}

/// The header and the IPC of the answer to `request`.
fn ask(connection: &Connection, request: &str) -> (Value, Vec<u8>) {
    let bytes = wait("the explorer's answer to a request", async {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(request.as_bytes()).await.unwrap();
        send.finish();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Some(n) = recv.read(&mut chunk).await.unwrap() {
            bytes.extend_from_slice(&chunk[..n]);
        }
        bytes
    });
    let (header, ipc) = answer(&bytes).unwrap();
    (header, ipc.to_vec())
}

/// An answer's header and its IPC, which starts 8-aligned: `[u32 header length][header][IPC]`,
/// the layout of a live frame's body too.
fn answer(bytes: &[u8]) -> Result<(Value, &[u8]), String> {
    let Some(len) = bytes.get(..4) else {
        return Err(format!("an answer of {} bytes has no header length", bytes.len()));
    };
    let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
    if (4 + len) % frame::ALIGN != 0 {
        return Err(format!("an answer's IPC starts at {}, not {}-aligned", 4 + len, frame::ALIGN));
    }
    let Some(head) = bytes.get(4..4 + len) else {
        return Err(format!("an answer of {} bytes is shorter than its header ({len})", bytes.len()));
    };
    let header: Value = serde_json::from_slice(head).map_err(|why| format!("an answer's header: {why}"))?;
    Ok((header, &bytes[4 + len..]))
}

/// The whole live frames at the front of `bytes` (`[u32 body length][body]`, the body as an
/// answer's), taken off it into `out`.
fn frames(bytes: &mut Vec<u8>, out: &mut Vec<(Value, Vec<u8>)>) -> Result<(), String> {
    // Bounded by `bytes`: each turn takes at least 4 of them, or ends.
    loop {
        let Some(len) = bytes.get(..4) else { return Ok(()) };
        let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
        let Some(body) = bytes.get(4..4 + len) else { return Ok(()) };
        let (header, ipc) = answer(body)?;
        out.push((header, ipc.to_vec()));
        bytes.drain(..4 + len);
    }
}

fn token_of(link: &str) -> String {
    let fragment = link.split_once('#').unwrap().1;
    let parts: Vec<&str> = fragment.split('/').collect();
    assert_eq!(parts.len(), 3, "{link}");
    parts[1].to_owned()
}

#[test]
fn a_session_is_refused_until_hello_then_answered_and_fed() {
    let explorer = explorer(false);
    let link = explorer.link();
    assert!(
        link.starts_with(&format!("https://ix.dev/explore#127.0.0.1:{}/", explorer.port())),
        "{link}"
    );
    assert!(link.ends_with(&format!("/{}", explorer.certhash())), "{link}");
    let token = token_of(&link);
    let client = client(&explorer);
    for (path, origin, status) in [
        (link::PATH, Some("https://evil.example"), 403),
        (link::PATH, Some(link::DEV_ORIGIN), 403),
        (link::PATH, None, 403),
        ("/elsewhere", Some(link::ORIGIN), 404),
    ] {
        let refused = connect(&client, &explorer, path, origin).map(|it| it.id());
        assert_eq!(refused, Err(Closed::Refused { status }), "{path} {origin:?}");
    }

    let connection = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).unwrap();
    let (header, ipc) = ask(&connection, r#"{"op":"count","n":3}"#);
    assert_eq!(header["error"]["kind"], "unauthenticated", "{header}");
    assert!(ipc.is_empty());

    let (header, _) = ask(&connection, &format!(r#"{{"op":"hello","token":"{token}"}}"#));
    assert_eq!(header["op"], "hello", "{header}");
    assert_eq!(header["schema"], 3);
    assert_eq!(header["certhash"], explorer.certhash().as_str());
    assert_eq!(header["live_every_ms"], 250);
    assert_eq!(header["source"]["extensions"][0], "test", "{header}");
    assert_eq!(header["protocol"], PROTOCOL);
    assert_eq!(header["layers"][0]["name"], "test", "{header}");
    assert_eq!(header["layers"][0]["version"], 3, "{header}");

    let (header, ipc) = ask(&connection, r#"{"op":"count","n":3}"#);
    assert_eq!(header["op"], "count", "{header}");
    assert_eq!(header["rows"], 3);
    assert!(header["elapsed_ns"].is_u64(), "{header}");
    assert_eq!(decode(&ipc).iter().map(RecordBatch::num_rows).sum::<usize>(), 3);

    let (header, _) = ask(&connection, r#"{"op":"count"}"#);
    assert_eq!(header["error"]["kind"], "bad_args", "the source's own error: {header}");
    let (header, _) = ask(&connection, r#"{"op":"frobnicate"}"#);
    assert_eq!(header["error"]["kind"], "unknown_op", "{header}");
    assert_eq!(header["error"]["op"], "frobnicate", "{header}");

    let mut live = wait("the explorer's live stream", connection.accept_uni()).unwrap();
    let (header, ipc) = wait("the live stream's first frame", async {
        let (mut bytes, mut got, mut chunk) = (Vec::new(), Vec::new(), [0u8; 4096]);
        while got.is_empty() {
            let n = live.read(&mut chunk).await.unwrap().unwrap();
            bytes.extend_from_slice(&chunk[..n]);
            frames(&mut bytes, &mut got).unwrap();
        }
        got.swap_remove(0)
    });
    let (op, table) = (header["op"].as_str(), header["table"].as_str());
    assert_eq!((op, table), (Some("live"), Some("spans")), "{header}");
    assert_eq!(header["rows"], 2, "{header}");
    assert_eq!(decode(&ipc).iter().map(RecordBatch::num_rows).sum::<usize>(), 2);

    let other = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).unwrap();
    let wrong = if token.starts_with('0') {
        token.replacen('0', "1", 1)
    } else {
        format!("0{}", &token[1..])
    };
    let (header, _) = ask(&other, &format!(r#"{{"op":"hello","token":"{wrong}"}}"#));
    assert_eq!(header["error"]["kind"], "bad_token", "{header}");
    let closed = wait("bad_token to close the session", other.closed());
    assert!(matches!(closed, Closed::ByPeer { code: CLOSE_BAD_TOKEN, .. }), "{closed}");
    let (header, _) = ask(&connection, r#"{"op":"count","n":1}"#);
    assert_eq!(header["op"], "count", "another session's bad token leaves this one open");
}

#[test]
fn a_session_from_the_dev_server_is_taken_only_when_asked() {
    let explorer = explorer(true);
    let client = client(&explorer);
    connect(&client, &explorer, link::PATH, Some(link::DEV_ORIGIN)).unwrap();
    connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).unwrap();
}

/// Dropping the explorer closes its sessions with [`CLOSE_STOPPED`] and its thread ends.
#[test]
fn dropping_the_explorer_closes_its_sessions() {
    let explorer = explorer(false);
    let client = client(&explorer);
    let connection = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).unwrap();
    drop(explorer);
    let closed = wait("the stop to close the session", connection.closed());
    assert!(matches!(closed, Closed::ByPeer { code: CLOSE_STOPPED, .. }), "{closed}");
}

// The server's machine under blaze-core's deterministic simulation: the server's [`Loop`]
// (tags from 0), its [`Serving`] (from `TAGS`), the page's client [`Loop`] (from `2 x TAGS`)
// and the jobs (from `3 x TAGS`, each a timer whose firing hands its result back after a
// delay drawn from the tape) on one [`Sim`], whose network loses, duplicates and reorders
// datagrams by the sim seed the tape gives. No socket, thread or real clock: every deadline is
// reached through an armed timer.

const NAME: &str = "test explore";
const EVERY: Duration = Duration::from_millis(250);
/// By when on the virtual clock the page's script is done and the server stopped: the late
/// request alone takes REQUEST_WAIT (10 s), the handshake and the other requests well under a
/// second, a lost packet's recovery a few probe timeouts more, and the stop SHUTDOWN (1 s).
const BOUND: Duration = Duration::from_secs(25);
/// How long past REQUEST_WAIT a late request's answer may take to reach the page.
const SLACK: Duration = Duration::from_secs(2);
/// The most turns one run takes: far more than the script needs, so a run not done by then is
/// one that stalled.
const TURNS_MOST: usize = 1_000_000;
const JOB_TAGS: u64 = 3 * h3_driver::TAGS;

fn server_at() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 4433))
}

/// What the page expects of an answer.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// An error of this kind.
    Error(&'static str),
    /// `unknown_op`, naming the op.
    UnknownOp(&'static str),
    Hello,
    /// `count`'s answer of this many rows.
    Rows(usize),
    /// `bad_args` for a request not whole in REQUEST_WAIT, answered then.
    Late,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    /// The session that says hello and is fed.
    A,
    /// The session whose hello has the wrong token.
    C,
}

/// The page's requests: A's in groups, each group asked once the one before it is answered
/// (the last also once C is closed); C's one as it opens.
const ASKS: [(Which, &[u8], bool, Expect); 8] = [
    (Which::A, br#"{"op":"count","n":3}"#, true, Expect::Error("unauthenticated")),
    (Which::A, GOOD, true, Expect::Hello),
    (Which::A, br#"{"op":"count","n":3}"#, true, Expect::Rows(3)),
    (Which::A, br#"{"op":"count"}"#, true, Expect::Error("bad_args")),
    (Which::A, br#"{"op":"frobnicate"}"#, true, Expect::UnknownOp("frobnicate")),
    // Never finished: answered late.
    (Which::A, br#"{"op":"cou"#, false, Expect::Late),
    (Which::A, br#"{"op":"count","n":1}"#, true, Expect::Rows(1)),
    (Which::C, WRONG, true, Expect::Error("bad_token")),
];
const GROUPS: [&[usize]; 5] = [&[0], &[1], &[2], &[3, 4, 5], &[6]];

/// What the page saw of one request.
#[derive(Default, Debug)]
struct Seen {
    opened_at: Option<Duration>,
    body: Vec<u8>,
    /// How many times it ended: finished, reset, or its session closed.
    ends: u32,
    finished: bool,
    ended_at: Option<Duration>,
    /// Something it told after it ended.
    after_end: bool,
}

/// The page: four sessions refused, A and C, and A's live stream.
struct Page {
    refused: Vec<(SessionId, u16, Option<Closed>)>,
    a: SessionId,
    c: SessionId,
    a_open: bool,
    a_closed: Option<Closed>,
    c_closed: Option<Closed>,
    seen: [Seen; ASKS.len()],
    streams: BTreeMap<StreamRef, usize>,
    /// A's next group.
    group: usize,
    /// Requests to open, waiting for the server to allow a stream.
    queued: VecDeque<usize>,
    live: Option<StreamRef>,
    live_bytes: Vec<u8>,
    /// Each live frame: when it came whole, its header and its IPC.
    live_frames: Vec<(Duration, Value, Vec<u8>)>,
}

fn connect_to(endpoint: &mut Endpoint, path: &str, origin: Option<&str>) -> Result<SessionId, String> {
    let url = format!("https://127.0.0.1:{}{path}", server_at().port());
    let headers: Vec<(&str, &str)> = origin.map(|it| ("origin", it)).into_iter().collect();
    endpoint.connect(server_at(), &url, &headers).map_err(|it| it.to_string())
}

impl Page {
    fn new(endpoint: &mut Endpoint) -> Result<Page, String> {
        let mut refused = Vec::new();
        for (path, origin, status) in [
            (link::PATH, Some("https://evil.example"), 403),
            (link::PATH, Some(link::DEV_ORIGIN), 403),
            (link::PATH, None, 403),
            ("/elsewhere", Some(link::ORIGIN), 404),
        ] {
            refused.push((connect_to(endpoint, path, origin)?, status, None));
        }
        Ok(Page {
            refused,
            a: connect_to(endpoint, link::PATH, Some(link::ORIGIN))?,
            c: connect_to(endpoint, link::PATH, Some(link::ORIGIN))?,
            a_open: false,
            a_closed: None,
            c_closed: None,
            seen: Default::default(),
            streams: BTreeMap::new(),
            group: 0,
            queued: VecDeque::new(),
            live: None,
            live_bytes: Vec::new(),
            live_frames: Vec::new(),
        })
    }

    fn session(&self, which: Which) -> SessionId {
        match which {
            Which::A => self.a,
            Which::C => self.c,
        }
    }

    fn seen(&mut self, stream: StreamRef) -> Option<&mut Seen> {
        let i = *self.streams.get(&stream)?;
        let seen = &mut self.seen[i];
        if seen.ends > 0 {
            seen.after_end = true;
        }
        Some(seen)
    }

    fn end(seen: &mut Seen, now: Duration, finished: bool) {
        seen.ends += 1;
        seen.finished |= finished && seen.ends == 1;
        seen.ended_at.get_or_insert(now);
    }

    /// Queues A's next groups that may be asked.
    fn advance(&mut self) {
        // Bounded by GROUPS: each turn queues one, or ends.
        while self.a_open && self.group < GROUPS.len() {
            let before_answered = self.group == 0 || GROUPS[self.group - 1].iter().all(|i| self.seen[*i].ends > 0);
            let last_waits = self.group == GROUPS.len() - 1 && self.c_closed.is_none();
            if !before_answered || last_waits {
                return;
            }
            self.queued.extend(GROUPS[self.group].iter().copied());
            self.group += 1;
        }
    }

    fn open(&mut self, endpoint: &mut Endpoint, now: Duration) -> Result<(), String> {
        while let Some(&i) = self.queued.front() {
            let (which, request, fin, _) = ASKS[i];
            let Some(stream) = endpoint.open(self.session(which), Direction::Bi).map_err(|it| it.to_string())? else {
                return Ok(());
            };
            self.queued.pop_front();
            self.streams.insert(stream, i);
            self.seen[i].opened_at = Some(now);
            endpoint.send(stream, request).map_err(|it| it.to_string())?;
            if fin {
                endpoint.finish(stream).map_err(|it| it.to_string())?;
            }
        }
        Ok(())
    }

    /// What the page saw since the last turn; a server's misstep is the oracle's finding, the
    /// client endpoint refusing the page's own call the harness's.
    fn events(&mut self, endpoint: &mut Endpoint, now: Duration) -> Result<(), Broke> {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionOpened { session } if session == self.a => {
                    self.a_open = true;
                    self.advance();
                }
                Event::SessionOpened { session } if session == self.c => self.queued.push_back(7),
                Event::SessionOpened { session } => {
                    return Err(Broke::Oracle(format!("session {session:?}, to be refused, opened")));
                }
                Event::SessionClosed { session, closed } => {
                    for (i, (which, ..)) in ASKS.iter().enumerate() {
                        if self.session(*which) == session && self.seen[i].ends == 0 && self.seen[i].opened_at.is_some() {
                            Page::end(&mut self.seen[i], now, false);
                        }
                    }
                    if session == self.a {
                        self.a_closed = Some(closed);
                    } else if session == self.c {
                        self.c_closed = Some(closed);
                        self.advance();
                    } else if let Some(refused) = self.refused.iter_mut().find(|it| it.0 == session) {
                        refused.2 = Some(closed);
                    }
                }
                Event::StreamOpened { session, stream } => {
                    if session != self.a || stream.direction() != Direction::Uni || self.live.is_some() {
                        return Err(Broke::Oracle(format!("the server opened stream {stream:?} of session {session:?}, which is not A's one live stream")));
                    }
                    self.live = Some(stream);
                }
                Event::StreamData { stream, bytes } => {
                    endpoint.consumed(stream, bytes.len());
                    if self.live == Some(stream) {
                        self.live_bytes.extend_from_slice(&bytes);
                        let mut got = Vec::new();
                        frames(&mut self.live_bytes, &mut got).map_err(|why| Broke::Oracle(format!("a live frame: {why}")))?;
                        self.live_frames.extend(got.into_iter().map(|(header, ipc)| (now, header, ipc)));
                    } else if let Some(seen) = self.seen(stream) {
                        seen.body.extend_from_slice(&bytes);
                    }
                }
                Event::StreamFinished { stream } => {
                    if let Some(seen) = self.seen(stream) {
                        Page::end(seen, now, true);
                    }
                    self.advance();
                }
                Event::StreamReset { stream, .. } => {
                    if let Some(seen) = self.seen(stream) {
                        Page::end(seen, now, false);
                    }
                    self.advance();
                }
                _ => {}
            }
        }
        self.open(endpoint, now).map_err(Broke::Harness)
    }

    /// Every request asked and ended.
    fn done(&self) -> bool {
        self.group == GROUPS.len() && self.queued.is_empty() && self.seen.iter().all(|it| it.ends > 0)
    }

    /// The oracle, over what the page saw by the end; `stop_at` when the server was stopped.
    fn check(&self, stop_at: Option<Duration>) -> Result<(), String> {
        for (session, status, closed) in &self.refused {
            if *closed != Some(Closed::Refused { status: *status }) {
                return Err(format!("session {session:?} was to be refused with {status}, and ended {closed:?}"));
            }
        }
        for (i, (_, request, _, expect)) in ASKS.iter().enumerate() {
            let seen = &self.seen[i];
            let request = String::from_utf8_lossy(request);
            if seen.ends != 1 || seen.after_end || !seen.finished {
                return Err(format!("request {i} ({request}) ended {} times, finished: {}, told after its end: {}", seen.ends, seen.finished, seen.after_end));
            }
            let (header, ipc) = answer(&seen.body).map_err(|why| format!("request {i} ({request}): {why}"))?;
            let rows = || decode(ipc).iter().map(RecordBatch::num_rows).sum::<usize>();
            let right = match *expect {
                Expect::Error(kind) => header["error"]["kind"] == kind && ipc.is_empty(),
                Expect::UnknownOp(op) => header["error"]["kind"] == "unknown_op" && header["error"]["op"] == op,
                Expect::Hello => {
                    header["op"] == "hello"
                        && header["live_every_ms"] == EVERY.as_millis() as u64
                        && header["protocol"] == PROTOCOL
                        && header["layers"][0]["name"] == "test"
                }
                Expect::Rows(n) => header["op"] == "count" && header["rows"] == n && rows() == n,
                Expect::Late => {
                    let (opened, ended) = (seen.opened_at.unwrap_or(Duration::ZERO), seen.ended_at.unwrap_or(Duration::MAX));
                    let took = ended.saturating_sub(opened);
                    header["error"]["kind"] == "bad_args" && took >= REQUEST_WAIT && took <= REQUEST_WAIT + SLACK
                }
            };
            if !right {
                let took = seen.ended_at.zip(seen.opened_at).map(|(ended, opened)| ended.saturating_sub(opened));
                return Err(format!("request {i} ({request}) was to be answered {expect:?}, and was answered {header} in {took:?}"));
            }
        }
        if !matches!(self.c_closed, Some(Closed::ByPeer { code: CLOSE_BAD_TOKEN, .. })) {
            return Err(format!("the bad token's session was to be closed with {CLOSE_BAD_TOKEN}, and ended {:?}", self.c_closed));
        }
        let Some(stop_at) = stop_at else { return Err("the page's script did not end".to_owned()) };
        if !matches!(self.a_closed, Some(Closed::ByPeer { code: CLOSE_STOPPED, .. })) {
            return Err(format!("the stop was to close session A with {CLOSE_STOPPED}, and it ended {:?}", self.a_closed));
        }
        self.check_live(stop_at)
    }

    /// Every live frame parses and holds the rows it says; the `spans` table comes a frame a
    /// tick from the first, at least every other period until the stop; `open` comes once with
    /// its row and once emptied.
    fn check_live(&self, stop_at: Duration) -> Result<(), String> {
        let Some((first_at, first, _)) = self.live_frames.first() else { return Err("no live frame came".to_owned()) };
        if first["table"] != "spans" || first["rows"] != 2 {
            return Err(format!("the first live frame was to be `spans` of 2 rows, and is {first}"));
        }
        let mut open = Vec::new();
        let mut spans = 0u64;
        for (_, header, ipc) in &self.live_frames {
            let rows = if ipc.is_empty() { 0 } else { decode(ipc).iter().map(RecordBatch::num_rows).sum::<usize>() };
            if header["op"] != "live" || header["rows"] != rows {
                return Err(format!("a live frame {header} carries {rows} rows"));
            }
            match header["table"].as_str() {
                Some("spans") => spans += 1,
                Some("open") => open.push(rows),
                _ => return Err(format!("a live frame of no table the source has: {header}")),
            }
        }
        if open != [1, 0] {
            return Err(format!("`open` was to come once with its row and once emptied, and came as {open:?}"));
        }
        let periods = (stop_at.saturating_sub(*first_at).as_nanos() / EVERY.as_nanos()) as u64;
        if spans < 8 || spans < periods / 2 {
            return Err(format!("{spans} `spans` frames came in {periods} periods of the live stream"));
        }
        Ok(())
    }
}

/// The network's faults and how long a job may take.
#[derive(Clone, Copy, Debug)]
struct Net {
    lose_one_in: u32,
    duplicate_one_in: u32,
    job_most_ms: u64,
}

/// One run of the tape: the page's script, then the stop, driven to their ends, then the
/// oracle. The tape gives the network's seed and each job's delay; what the page or the server
/// got wrong is `Broke::Oracle`, the test's own setup or bookkeeping failing `Broke::Harness`.
fn run(tape: &Tape, net: Net, plant: Plant) -> Result<(), Broke> {
    let seed = tape.next_u64("sim seed");
    let prefix = format!("sim seed {seed}, {net:?}, {plant:?}");
    let harness = |why: String| Broke::Harness(format!("{prefix}: {why}"));
    let oracle = |why: String| Broke::Oracle(format!("{prefix}: {why}"));
    let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_millis(1), ..Faults::default() });
    sim.datagrams(Datagrams { lose_one_in: net.lose_one_in, duplicate_one_in: net.duplicate_one_in, max_delay: Duration::from_millis(5) });
    let epoch = Instant::now();
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let rotation = Rotation::new(now_s);
    let cert = Cert::make(rotation.current(), NAME).map_err(|it| harness(it.to_string()))?;
    let identity = cert.identity().map_err(|it| harness(it.to_string()))?;
    let pin = blaze_sha256::digest(&cert.der);
    let server_config = ServerConfig { identity, transport: Transport::default(), requests: false };
    let server = Endpoint::new(epoch, Some(server_config), None).map_err(|it| harness(it.to_string()))?;
    let client_config = ClientConfig { trust: Trust::Pinned(vec![pin]), transport: Transport::default() };
    let client = Endpoint::new(epoch, None, Some(client_config)).map_err(|it| harness(it.to_string()))?;
    let mut server = Loop::new(server, 0, server_at());
    let mut client = Loop::new(client, 2 * h3_driver::TAGS, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let clock = |now: Duration| Clock { now, now_ns: now.as_nanos() as i64, wall_s: now_s + now.as_secs() };
    let setup = Setup {
        token: Token::from_bytes(TOKEN),
        origins: Origins { dev: false },
        name: NAME,
        every: EVERY,
        served: Served { hash: cert.hash, not_after_s: cert.validity.not_after_s },
        rotation,
        core: Err("the test serves no core".to_owned()),
    };
    let mut serving = Serving::new(setup, serving::TAGS, clock(sim.now()));
    serving.plant(plant);
    let mut page = Page::new(client.endpoint()).map_err(harness)?;
    let (mut effects, mut done, mut jobs, mut said) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut waiting: BTreeMap<u64, (JobId, Done)> = BTreeMap::new();
    let mut next_job = 0;
    let (mut stop_at, mut over) = (None, None);
    for _ in 0..TURNS_MOST {
        let now = sim.now();
        if now > BOUND || (over.is_some() && page.a_closed.is_some()) {
            break;
        }
        server.endpoint().advance_clock(now);
        client.endpoint().advance_clock(now);
        serving.step(clock(now), server.endpoint(), &mut effects);
        serving.take_jobs(&mut jobs);
        for job in jobs.drain(..) {
            let now_ns = now.as_nanos() as i64;
            let result = work_on(job.work, &Test, &move || now_ns);
            let after = Duration::from_millis(tape.int_in(0..=net.job_most_ms, "job delay ms"));
            let tag = JOB_TAGS + next_job;
            next_job += 1;
            waiting.insert(tag, (job.id, result));
            effects.push(Effect::Timer { after, tag: Tag(tag) });
        }
        serving.take_said(&mut said);
        page.events(client.endpoint(), now).map_err(|broke| match broke {
            Broke::Oracle(why) => oracle(why),
            Broke::Harness(why) => harness(why),
        })?;
        if stop_at.is_none() && page.done() {
            serving.stop(now, format!("{NAME} stopped"));
            stop_at = Some(now);
        }
        if over.is_none() {
            over = serving.over(server.endpoint());
        }
        server.step(now, &mut effects);
        client.step(now, &mut effects);
        sim.submit(&mut effects);
        sim.poll(&mut done, server.deadline().min(client.deadline()).min(serving.deadline()));
        let now = sim.now();
        server.endpoint().advance_clock(now);
        client.endpoint().advance_clock(now);
        for completion in done.drain(..) {
            if server.owns(completion.tag) {
                server.complete(completion);
            } else if serving.owns(completion.tag) {
                serving.complete(completion);
            } else if client.owns(completion.tag) {
                client.complete(completion);
            } else if let Some((id, result)) = waiting.remove(&completion.tag.0) {
                serving.done(id, result);
            } else {
                return Err(harness(format!("{:?} is nobody's", completion.tag)));
            }
        }
        if let Some(failed) = server.failed().or(client.failed()) {
            return Err(harness(format!("a loop's socket failed: {failed}")));
        }
    }
    if !said.is_empty() {
        return Err(oracle(format!("the server said {said:?}")));
    }
    page.check(stop_at).map_err(oracle)?;
    if over.is_none() {
        return Err(oracle("the stopped server was never over".to_owned()));
    }
    Ok(())
}

/// The invariant: every request the page opens ends exactly once, finished, with the answer
/// its rule says (`unauthenticated` before hello, hello, the source's rows, `bad_args`,
/// `unknown_op` naming the op, a late request's `bad_args` REQUEST_WAIT after it opened, and
/// `bad_token` closing that session alone, and only after the page has the whole answer and
/// its FIN: the close waits for the answer's acknowledgement (`Event::StreamAcked`), not for a
/// margin of time, so it holds however late loss makes the answer); every refused CONNECT gets its status; the live
/// stream's frames all parse and come at least every other period; and the stop closes every
/// session with CLOSE_STOPPED and ends the machine: whether the network is clean or loses,
/// duplicates and reorders datagrams, and whatever each job takes.
#[test]
fn every_request_is_answered_once_by_its_rule_and_the_live_stream_keeps_its_period() {
    blaze_sim::check(blaze_sim::Config::cases(16), |tape| {
        let lossy = tape.bool("lossy");
        let lose_one_in = if lossy { tape.int_in(8..=32u32, "lose one in") } else { 0 };
        let duplicate_one_in = if lossy { 8 } else { 0 };
        let net = Net { lose_one_in, duplicate_one_in, job_most_ms: tape.int_in(0..=100u64, "job most ms") };
        run(tape, net, Plant::None)
    });
}

const CONTROL_SEEDS: u64 = 4;
const CLEAN: Net = Net { lose_one_in: 0, duplicate_one_in: 0, job_most_ms: 20 };

/// `plant` caught by the oracle in every one of the tapes of seeds `0..CONTROL_SEEDS` on the
/// clean network, each tape's clean run passing: one control a seed, each of one case, so
/// every seed has to catch it, not just some.
fn caught_on_every_seed(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        let config = blaze_sim::Config::cases(1).first(seed);
        blaze_sim::control(config, Plant::None, plant, "", |tape, plant| run(tape, CLEAN, plant));
    }
}

/// Control: a job's result dropped (the hello's, a tick's or an answer's, by order of coming
/// back) leaves its request unanswered or the live stream unfed, and the oracle says so.
#[test]
fn the_oracle_catches_a_dropped_job_result() {
    for nth in 1..=3 {
        caught_on_every_seed(Plant::DropDone { nth });
    }
}

/// Control: with no timer armed the machine reaches its deadlines only when the network steps
/// it: the live stream ticks far less than its period and the late request is answered late,
/// and the oracle says so.
#[test]
fn the_oracle_catches_a_timer_never_armed() {
    caught_on_every_seed(Plant::NoRearm);
}
