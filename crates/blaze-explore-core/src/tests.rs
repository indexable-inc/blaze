//! The core read against the daemon's own producers: blaze-profile's `batches::open` and
//! `live::Encoder` make the tables' IPC, blaze-explore's `tasks::Feed` the tasks', ix-prof's
//! `explore::frame` makes the heads.

use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray, UInt16Array, UInt32Array, UInt64Array};
use arrow_ipc::{reader::StreamDecoder, writer::StreamWriter};
use arrow_schema::{DataType, Field, Schema};
use ix_prof::explore::{Tick, frame};
use blaze_profile::{
    batches::{self, Process},
    flight::{OpenSpan, ThreadNow},
    live::{Cap, Encoder},
    schema,
};
use blaze_protocol::cbor;
use serde_json::{Map, Value, json};

use crate::{
    abi,
    live::{Decoder, END},
    state::Session,
    view::{self, RunningNow, Which},
    wire,
};

const S: i64 = 1_000_000_000;
const PROCESS: Process<'static> = Process { pid: 7, name: "blaze serve", commit: None };

fn object(value: Value) -> Map<String, Value> {
    let Value::Object(map) = value else { unreachable!("a test's header is an object") };
    map
}

/// The daemon's hello, as its first answer, the clock at `now_ns`.
fn hello(now_ns: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let header = object(json!({
        "op": "hello",
        "daemon": "0.0.0-test",
        "schema": schema::VERSION,
        "clock": "test CLOCK_MONOTONIC",
        "now_ns": now_ns,
        "certhash": "ab12",
        "cert_not_after_unix_s": 1_800_000_000u64,
        "live_every_ms": 250,
        "source": { "kind": "blaze", "extensions": ["blaze"] },
    }));
    frame::head(&mut out, &header).unwrap();
    out
}

/// A live frame of `table`, its length first, as the daemon writes it.
fn live_frame(table: &str, rows: usize, at_ns: i64, ipc: &[u8]) -> Vec<u8> {
    let header = object(json!({
        "op": "live", "table": table, "rows": rows, "at_ns": at_ns, "skipped": 0, "lost": 0,
    }));
    let mut out = Vec::new();
    frame::live_head(&mut out, &header, ipc.len()).unwrap();
    out.extend_from_slice(ipc);
    out
}

fn span(name: &'static str, target: &'static str, for_ns: i64) -> OpenSpan {
    OpenSpan { name, target, for_ns: for_ns as u64, ..OpenSpan::default() }
}

fn thread(tid: u64, open: Vec<OpenSpan>) -> ThreadNow {
    ThreadNow { tid, name: format!("blaze-work-{tid}"), open, deeper: 0 }
}

/// Three frames of `open` from one encoder whose dictionaries hold 6 values: the first names 4,
/// the second adds 2 as deltas on the same stream, and the third, the dictionaries full,
/// starts with END and a new stream.
fn three_frames() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut encoder = Encoder::with_cap(&schema::OPEN, Cap { values: 6, bytes: 1 << 20 });
    let query = || OpenSpan {
        krate: Some("serde v1.0.219 (lib)".into()),
        function: Some("serde::de::Deserialize".into()),
        ..span("typeck_query", "blaze", S)
    };
    let ticks = [
        // blaze-work-0, blaze-work-1; job; blaze: 4 values.
        (10 * S, vec![thread(0, vec![span("job", "blaze", 3 * S)]), thread(1, vec![span("job", "blaze", S)])]),
        // typeck_query; serde: 2 more, and blaze-work-1 is inside nothing.
        (11 * S, vec![thread(0, vec![span("job", "blaze", 4 * S), query()])]),
        // Full: a new stream, blaze-work-1 between jobs in the pool's `worker` span.
        (
            12 * S,
            vec![
                thread(0, vec![span("job", "blaze", 5 * S), query()]),
                thread(
                    1,
                    vec![OpenSpan { function: Some("idle".into()), ..span("worker", "blaze", S / 2) }],
                ),
            ],
        ),
    ];
    let (mut frames, mut ipcs) = (Vec::new(), Vec::new());
    for (at_ns, threads) in ticks {
        let batch = batches::open(&threads, at_ns, PROCESS).unwrap();
        let mut ipc = Vec::new();
        encoder.encode(&batch, &mut ipc).unwrap();
        frames.push(live_frame("open", batch.num_rows(), at_ns, &ipc));
        ipcs.push(ipc);
    }
    assert_eq!(encoder.streams(), 2);
    (frames, ipcs)
}

#[test]
fn a_live_stream_across_three_frames_follows_deltas_and_the_reset_at_end() {
    let (frames, ipcs) = three_frames();
    assert!(!ipcs[1].starts_with(&END), "the second frame continues the stream");
    assert!(ipcs[2].starts_with(&END), "the third frame starts a new stream");
    // The second frame's dictionaries are deltas on the first's: alone, it does not read.
    let mut back = Vec::new();
    assert!(Decoder::<StreamDecoder>::new("open").decode(&ipcs[1], &mut back).is_err());

    let mut session = Session::<StreamDecoder>::new();
    session.feed_hello(&hello((9 * S) as u64)).unwrap();
    // The stream as it arrives: the frames back to back, cut into 7-byte reads.
    let stream: Vec<u8> = frames.concat();
    for (i, chunk) in stream.chunks(7).enumerate() {
        session.feed_live_stream(chunk).unwrap();
        if i == 0 {
            assert_eq!(session.stats().frames, 0);
        }
    }
    assert_eq!(session.stats().frames, 3);
    assert_eq!(session.streams()[0], 2, "the reset began a second stream");

    let now = view::running_now(&mut session, 12 * S + S / 2).unwrap();
    assert_eq!((now.workers, now.busy_count, now.idle), (2, 1, 1), "{now:?}");
    assert_eq!(now.headline, "1 of 2 busy · 1 idle");
    assert_eq!(now.idle_line, "1 idle worker");
    let busy = &now.busy[0];
    assert_eq!(busy.thread_name, "blaze-work-0");
    assert_eq!(busy.breadcrumb, "serde › typeck_query › serde::de::Deserialize");
    assert_eq!((busy.for_ms, busy.for_text.as_str()), (5_500, "5.5s"));
    assert_eq!(busy.stack.len(), 2);
    assert_eq!(busy.stack[1].krate.as_deref(), Some("serde v1.0.219 (lib)"));
    assert!(now.internal.is_empty());
    assert_eq!(now.phase, "running", "no tasks: busy and idle from the open spans alone");
    assert_eq!(now.tally.total, 0);
    assert!(now.blockers.is_empty() && now.downloads.is_empty());

    // A frame with no rows: the open set emptied.
    session.feed_live_frame(&live_frame("open", 0, 13 * S, &[])[wire::HEAD..]).unwrap();
    let now = view::running_now(&mut session, 13 * S).unwrap();
    assert_eq!((now.workers, now.busy_count, now.idle), (2, 0, 2), "{now:?}");
}

#[test]
fn ids_past_two_to_the_53_survive_to_the_view() {
    let thread = (1u64 << 53) + 1;
    let owner = (1u64 << 63) + 5;
    let span_id = u64::MAX;
    let fields = vec![
        Field::new("process", DataType::UInt32, false),
        Field::new("thread", DataType::UInt64, false),
        Field::new("thread_name", DataType::Utf8, false),
        Field::new("depth", DataType::UInt16, false),
        Field::new("span_id", DataType::UInt64, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, true),
        Field::new("start_ns", DataType::Int64, false),
        Field::new("owner", DataType::UInt64, true),
    ];
    let schema = Arc::new(Schema::new(fields));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(vec![1])),
        Arc::new(UInt64Array::from(vec![thread])),
        // A pool thread's name: under the `blaze` layer, any other is the daemon's own.
        Arc::new(StringArray::from(vec!["blaze-fiber-2"])),
        Arc::new(UInt16Array::from(vec![0])),
        Arc::new(UInt64Array::from(vec![Some(span_id)])),
        Arc::new(StringArray::from(vec!["load"])),
        Arc::new(StringArray::from(vec![Some("app")])),
        Arc::new(Int64Array::from(vec![2 * S])),
        Arc::new(UInt64Array::from(vec![Some(owner)])),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut ipc = Vec::new();
    let mut writer = StreamWriter::try_new(&mut ipc, &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();
    drop(writer);

    let mut session = Session::<StreamDecoder>::new();
    session.feed_hello(&hello((2 * S) as u64)).unwrap();
    session.feed_live_frame(&live_frame("open", 1, 3 * S, &ipc)[wire::HEAD..]).unwrap();
    let mut out = Vec::new();
    view::render(&mut session, Which::RunningNow, 3 * S, &mut out).unwrap();
    let now: RunningNow = cbor::from_slice(&out).unwrap();
    assert_eq!(now, view::running_now(&mut session, 3 * S).unwrap());
    assert_eq!(now.busy.len(), 1, "{now:?}");
    let row = &now.busy[0];
    assert_eq!(row.thread, "9007199254740993");
    assert_eq!(row.key, "1:9007199254740993");
    assert_eq!(row.stack[0].owner.as_deref(), Some("9223372036854775813"));
    assert_eq!(row.stack[0].span.as_deref(), Some("18446744073709551615"));
    // The same bytes render the same CBOR.
    let mut again = Vec::new();
    view::render(&mut session, Which::RunningNow, 3 * S, &mut again).unwrap();
    assert_eq!(out, again);
}

/// `bytes` fed through the ABI, as the page would: copied into memory the module gave.
#[allow(unsafe_code)]
fn feed(handle: u32, kind: u32, bytes: &[u8]) -> i32 {
    let ptr = abi::explore_alloc(bytes.len());
    assert!(!ptr.is_null());
    // SAFETY: `ptr` is `bytes.len()` bytes from `explore_alloc`, not overlapping `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
    // SAFETY: `ptr` is readable for `bytes.len()` bytes and nothing writes it during the call.
    let status = unsafe { abi::explore_feed(handle, kind, ptr, bytes.len()) };
    // SAFETY: `ptr` is `explore_alloc(bytes.len())`'s, given back once.
    unsafe { abi::explore_free(ptr, bytes.len()) };
    status
}

#[test]
fn the_abi_round_trips_a_session() {
    assert_eq!(abi::explore_abi(), abi::ABI);
    let handle = abi::explore_new();
    assert_ne!(handle, 0);
    assert_eq!(feed(handle, 1, &hello((9 * S) as u64)), abi::OK);
    let (frames, _) = three_frames();
    assert_eq!(feed(handle, 3, &frames.concat()), abi::OK, "{}", abi::out_and_error().1);

    assert_eq!(abi::explore_view(handle, Which::RunningNow as u32, 12 * S), abi::OK);
    assert_eq!(abi::explore_out_len(), abi::out_and_error().0.len());
    let now: RunningNow = cbor::from_slice(&abi::out_and_error().0).unwrap();
    assert_eq!((now.workers, now.busy_count), (2, 1));
    for which in [Which::Recent, Which::Counters, Which::Session] {
        assert_eq!(abi::explore_view(handle, which as u32, 12 * S), abi::OK, "{which:?}");
    }

    // Refusals say what and why.
    assert_eq!(feed(handle, 9, b"x"), abi::BAD_ARG);
    assert!(abi::out_and_error().1.contains("kind 9"));
    assert_eq!(abi::explore_view(handle + 1, 1, 0), abi::NO_HANDLE);
    assert!(abi::out_and_error().0.is_empty());
    assert_eq!(feed(handle, 1, &hello(0)), abi::ERROR, "a second hello");
    assert!(abi::out_and_error().1.starts_with("hello: a second hello"));
    assert_eq!(feed(handle, 2, b"\x02\0\0\0{}"), abi::ERROR);
    assert!(abi::out_and_error().1.contains("field `op`"), "{}", abi::out_and_error().1);
    assert_eq!(abi::explore_error_len(), abi::out_and_error().1.len());

    assert_eq!(abi::explore_drop(handle), abi::OK);
    assert_eq!(abi::explore_drop(handle), abi::NO_HANDLE);
}

#[test]
fn the_core_reads_the_daemon_schema() {
    assert!(wire::SCHEMAS.contains(&schema::VERSION));
    let mut other = Vec::new();
    let header = object(json!({
        "op": "hello", "daemon": "0", "schema": 999, "clock": "c", "now_ns": 0, "certhash": "",
        "cert_not_after_unix_s": 0, "live_every_ms": 250, "source": { "extensions": [] },
    }));
    frame::head(&mut other, &header).unwrap();
    let refused = wire::hello(&other).unwrap_err().to_string();
    assert!(refused.starts_with("hello, field `schema`: is 999"), "{refused}");
}

#[test]
fn a_live_header_error_names_the_table_and_field() {
    let header = object(json!({
        "op": "live", "table": "open", "rows": 1, "at_ns": "soon", "skipped": 0, "lost": 0,
    }));
    let mut out = Vec::new();
    frame::live_head(&mut out, &header, 8).unwrap();
    out.extend_from_slice(&[0; 8]);
    let refused = wire::live(&out[wire::HEAD..]).unwrap_err().to_string();
    assert_eq!(
        refused,
        "live frame for table `open`, field `at_ns`: holds \"soon\", not a signed 64-bit integer"
    );
}

#[test]
fn the_daemons_tasks_fold_through_the_one_model_into_running_now() {
    use blaze_explore::tasks::{Feed, Tasks, task_id};
    use blaze_protocol::{
        events::NodeState,
        now::{Record, State},
    };
    let tasks = Arc::new(Tasks::new());
    // The plane's clock is the profile's.
    tasks.clock(0, 0);
    {
        let mut table = tasks.lock();
        let at = S as u64;
        table.said(7, at, Record::Task { id: 1, kind: "codegen", name: "serde lib", deps: &[] });
        table.said(7, at, Record::Task { id: 2, kind: "link", name: "app bin", deps: &[1] });
        let running = Record::Moved { id: 1, state: State::Running, node: &NodeState::Running };
        table.said(7, 2 * at, running);
    }
    let mut feed = Feed::new(Arc::clone(&tasks));
    let mut tick = Tick::default();
    tick.clear(3 * S);
    feed.tick(3 * S, &[], &mut tick).unwrap();
    let names: Vec<_> = tick.tables.iter().map(|it| (it.name, it.rows)).collect();
    assert_eq!(names, [("tasks", 2), ("task_edges", 1)]);

    let mut session = Session::<StreamDecoder>::new();
    session.feed_hello(&hello(0)).unwrap();
    for table in &tick.tables {
        let frame = live_frame(table.name, table.rows, 3 * S, &table.ipc);
        session.feed_live_frame(&frame[wire::HEAD..]).unwrap();
    }
    let now = view::running_now(&mut session, 4 * S).unwrap();
    assert_eq!(now.phase, "compiling", "{now:?}");
    assert_eq!((now.tally.total, now.tally.running, now.tally.waiting), (2, 1, 1));
    assert_eq!(now.blockers.len(), 1, "{now:?}");
    let blocker = &now.blockers[0];
    assert_eq!(blocker.task.name, "serde lib");
    assert_eq!(blocker.task.task, Some(task_id(7, 1).to_string()), "ids as decimal strings");
    assert_eq!((blocker.holds, blocker.chain), (1, 1));
    assert_eq!(blocker.for_ms, 2_000, "running since its start_ns, not since the page saw it");

    // A second tick sends only what changed: nothing.
    let mut again = Tick::default();
    again.clear(5 * S);
    feed.tick(5 * S, &[], &mut again).unwrap();
    assert!(again.tables.iter().all(|it| it.rows == 0 && it.ipc.is_empty()), "{again:?}");
}
