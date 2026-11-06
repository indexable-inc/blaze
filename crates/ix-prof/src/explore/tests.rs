//! The session core on a clock of the tests' own, and the server end to end over a test
//! source: the CONNECT rules, hello first, the token, an answer, an unknown op and the live
//! stream, as the page sees them.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use arrow_array::{Int64Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use arrow_schema::{DataType, Field, Schema};
use base64::Engine as _;
use blaze_http::h3::{
    ClientConfig, Closed, Transport,
    tokio_driver::{Driver, Session as Connection},
};
use serde_json::Value;

use super::{
    Answer, Config, Explorer, Hello, Kind, Layer, Live, OpError, PROTOCOL, Rule, Source, Stop,
    Table, Tick, frame, link,
    link::Token,
    serve,
    session::{
        self, CLOSE_BAD_TOKEN, Emit, Feed, MAX_IN_FLIGHT, Phase, Served, Session, Step,
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
    match core.request(&token, br#"{"op":"count","n":3}"#) {
        Step::Op { name, args } => {
            assert_eq!(name, "count");
            assert_eq!(args["n"], 3, "the source is given the request whole");
            assert_eq!(args["op"], "count");
        }
        other => panic!("the source's op, not {other:?}"),
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
    let header = session::hello_header(&hello(), &served, Duration::from_millis(250), None);
    let header = Value::Object(header);
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
    let header = session::hello_header(&hello(), &served, Duration::from_millis(250), Some(&core));
    let header = Value::Object(header);
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
    let header = OpError::new("sql", "no such table").with("query", "select * from x").header();
    let error = &header["error"];
    assert_eq!(error["kind"], "sql");
    assert_eq!(error["message"], "no such table");
    assert_eq!(error["query"], "select * from x");
    assert_eq!(header.len(), 1);
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

/// A client pinning the explorer's certificate, as the page does; on the runtime it is
/// called on.
fn client(explorer: &Explorer) -> Driver {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(explorer.certhash()).unwrap();
    let config =
        ClientConfig { pins: vec![raw.try_into().unwrap()], transport: Transport::default() };
    let socket = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    Driver::spawn(&tokio::runtime::Handle::current(), socket, None, Some(config)).unwrap().0
}

async fn connect(
    client: &Driver,
    explorer: &Explorer,
    path: &str,
    origin: Option<&str>,
) -> Result<Connection, Closed> {
    let url = format!("https://127.0.0.1:{}{path}", explorer.port());
    let to = SocketAddr::from((Ipv4Addr::LOCALHOST, explorer.port()));
    let headers: Vec<(&str, &str)> = origin.map(|it| ("origin", it)).into_iter().collect();
    client.connect(to, &url, &headers).await
}

/// The header and the IPC of the answer to `request`.
async fn ask(connection: &Connection, request: &str) -> (Value, Vec<u8>) {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(request.as_bytes()).await.unwrap();
    send.finish();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Some(n) = recv.read(&mut chunk).await.unwrap() {
        bytes.extend_from_slice(&chunk[..n]);
    }
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!((4 + len) % frame::ALIGN, 0, "the IPC starts 8-aligned");
    let header: Value = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
    (header, bytes[4 + len..].to_vec())
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
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let client = client(&explorer);
        for (path, origin, status) in [
            (link::PATH, Some("https://evil.example"), 403),
            (link::PATH, Some(link::DEV_ORIGIN), 403),
            (link::PATH, None, 403),
            ("/elsewhere", Some(link::ORIGIN), 404),
        ] {
            let refused = connect(&client, &explorer, path, origin).await.map(|it| it.id());
            assert_eq!(refused, Err(Closed::Refused { status }), "{path} {origin:?}");
        }

        let connection = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
        let (header, ipc) = ask(&connection, r#"{"op":"count","n":3}"#).await;
        assert_eq!(header["error"]["kind"], "unauthenticated", "{header}");
        assert!(ipc.is_empty());

        let (header, _) = ask(&connection, &format!(r#"{{"op":"hello","token":"{token}"}}"#)).await;
        assert_eq!(header["op"], "hello", "{header}");
        assert_eq!(header["schema"], 3);
        assert_eq!(header["certhash"], explorer.certhash().as_str());
        assert_eq!(header["live_every_ms"], 250);
        assert_eq!(header["source"]["extensions"][0], "test", "{header}");
        assert_eq!(header["protocol"], PROTOCOL);
        assert_eq!(header["layers"][0]["name"], "test", "{header}");
        assert_eq!(header["layers"][0]["version"], 3, "{header}");

        let (header, ipc) = ask(&connection, r#"{"op":"count","n":3}"#).await;
        assert_eq!(header["op"], "count", "{header}");
        assert_eq!(header["rows"], 3);
        assert!(header["elapsed_ns"].is_u64(), "{header}");
        assert_eq!(decode(&ipc).iter().map(RecordBatch::num_rows).sum::<usize>(), 3);

        let (header, _) = ask(&connection, r#"{"op":"count"}"#).await;
        assert_eq!(header["error"]["kind"], "bad_args", "the source's own error: {header}");
        let (header, _) = ask(&connection, r#"{"op":"frobnicate"}"#).await;
        assert_eq!(header["error"]["kind"], "unknown_op", "{header}");
        assert_eq!(header["error"]["op"], "frobnicate", "{header}");

        let mut live = connection.accept_uni().await.unwrap();
        let mut first = Vec::new();
        let mut chunk = [0u8; 4096];
        let read = async {
            loop {
                let n = live.read(&mut chunk).await.unwrap().unwrap();
                first.extend_from_slice(&chunk[..n]);
                let len = |it: &[u8]| u32::from_le_bytes(it[..4].try_into().unwrap()) as usize;
                if first.len() >= 4 && first.len() >= 4 + len(&first) {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), read).await.expect("a live frame within 5 s");
        let body_len = u32::from_le_bytes(first[..4].try_into().unwrap()) as usize;
        let body = &first[4..4 + body_len];
        let head_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
        assert_eq!((4 + head_len) % frame::ALIGN, 0, "a live frame's IPC is 8-aligned in its body");
        let header: Value = serde_json::from_slice(&body[4..4 + head_len]).unwrap();
        let (op, table) = (header["op"].as_str(), header["table"].as_str());
        assert_eq!((op, table), (Some("live"), Some("spans")), "{header}");
        assert_eq!(header["rows"], 2, "{header}");
        let batches = decode(&body[4 + head_len..]);
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);

        let other = connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
        let wrong = if token.starts_with('0') {
            token.replacen('0', "1", 1)
        } else {
            format!("0{}", &token[1..])
        };
        let (header, _) = ask(&other, &format!(r#"{{"op":"hello","token":"{wrong}"}}"#)).await;
        assert_eq!(header["error"]["kind"], "bad_token", "{header}");
        tokio::time::timeout(Duration::from_secs(5), other.closed())
            .await
            .expect("bad_token closes the session");
        let (header, _) = ask(&connection, r#"{"op":"count","n":1}"#).await;
        assert_eq!(header["op"], "count", "another session's bad token leaves this one open");
    });
}

#[test]
fn a_session_from_the_dev_server_is_taken_only_when_asked() {
    let explorer = explorer(true);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let client = client(&explorer);
        connect(&client, &explorer, link::PATH, Some(link::DEV_ORIGIN)).await.unwrap();
        connect(&client, &explorer, link::PATH, Some(link::ORIGIN)).await.unwrap();
    });
}
