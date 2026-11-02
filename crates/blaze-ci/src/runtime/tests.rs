use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    sync::Arc,
    thread,
    time::Duration,
};

use blaze_api::http;
use blaze_ids::ContentId;
use blaze_vfs::{Mode, Op};
use parking_lot::Mutex;

use super::{
    GITHUB_RETRIES, Post, Posts, batches, listen, load, post, save, waited,
    listen::{Listener, Seen},
};
use crate::{
    CheckRun, CheckStatus, Checkout, Event,
    config::Secret,
    github::{Checks, GitHubError},
    service::{Held, Input, RunId, State},
    snapshot::Kept,
};

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("blaze-ci-runtime-{}-{name}", std::process::id()))
}

fn cost(op: &Op) -> usize {
    match op {
        Op::Write { path, bytes, .. } | Op::Edit { path, bytes, .. } => path.len() + bytes.len() + super::OP_OVERHEAD,
        Op::Delete { path } => path.len() + super::OP_OVERHEAD,
        Op::Rename { from, to } => from.len() + to.len() + super::OP_OVERHEAD,
    }
}

#[test]
fn no_ops_are_one_empty_batch_and_small_ops_one_batch() {
    assert_eq!(batches(Vec::new(), 1000).unwrap(), vec![Vec::<Op>::new()]);
    let ops = vec![
        Op::Write { path: "a".to_owned(), bytes: vec![1; 10], mode: Mode::Regular },
        Op::Delete { path: "b".to_owned() },
        Op::Rename { from: "c".to_owned(), to: "d".to_owned() },
    ];
    assert_eq!(batches(ops.clone(), 1000).unwrap(), vec![ops]);
}

#[test]
fn batches_stay_under_the_bound_and_keep_the_order() {
    let ops: Vec<Op> = (0..50).map(|n| Op::Write { path: format!("f{n:02}"), bytes: vec![n; 100], mode: Mode::Regular }).collect();
    let cut = batches(ops.clone(), 500).unwrap();
    assert!(cut.len() > 1);
    for batch in &cut {
        assert!(!batch.is_empty());
        assert!(batch.iter().map(cost).sum::<usize>() <= 500);
    }
    assert_eq!(cut.concat(), ops);
}

#[test]
fn a_file_too_large_for_one_request_is_written_then_appended() {
    let bytes: Vec<u8> = (0..2500u32).map(|n| n as u8).collect();
    let ops = vec![
        Op::Delete { path: "gone".to_owned() },
        Op::Write { path: "big".to_owned(), bytes: bytes.clone(), mode: Mode::Executable },
        Op::Write { path: "small".to_owned(), bytes: vec![7], mode: Mode::Regular },
    ];
    let cut = batches(ops.clone(), 1000).unwrap();
    let flat = cut.concat();
    assert_eq!(flat[0], Op::Delete { path: "gone".to_owned() });
    let mut rebuilt = Vec::new();
    for op in &flat[1..flat.len() - 1] {
        match op {
            Op::Write { path, bytes, mode } => {
                assert_eq!(path, "big");
                assert_eq!(*mode, Mode::Executable);
                assert!(rebuilt.is_empty());
                rebuilt.extend_from_slice(bytes);
            }
            Op::Edit { path, at, len, bytes } => {
                assert_eq!(path, "big");
                assert_eq!((*at, *len), (rebuilt.len() as u64, 0));
                rebuilt.extend_from_slice(bytes);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(rebuilt, bytes);
    assert_eq!(flat.last(), Some(&Op::Write { path: "small".to_owned(), bytes: vec![7], mode: Mode::Regular }));
    for batch in &cut {
        assert!(batch.iter().map(cost).sum::<usize>() <= 1000, "{batch:?}");
    }
}

#[test]
fn a_path_longer_than_a_request_is_refused_naming_it() {
    let ops = vec![Op::Write { path: "p".repeat(2000), bytes: vec![0; 10], mode: Mode::Regular }];
    assert!(batches(ops.clone(), 1000).unwrap_err().contains("ppp"));
}

#[test]
fn the_state_file_round_trips_and_is_empty_at_first() {
    let path = temp("state.json");
    let _ = std::fs::remove_file(&path);
    assert_eq!(load(&path).unwrap(), State::default());
    let mut state = State::default();
    state.checkouts.insert(
        "branch/main".to_owned(),
        Held { checkout: Checkout { commit: "c".to_owned(), root: ContentId([9; 32]) }, at: 4 },
    );
    state.snapshots.push(Kept { id: "s".to_owned(), name: "n".to_owned(), at: 5 });
    save(&path, &state).unwrap();
    assert_eq!(load(&path).unwrap(), state);
    std::fs::write(&path, "{\"checkouts\": {}, \"snapshots\": [], \"other\": 1}").unwrap();
    let error = load(&path).unwrap_err();
    assert!(error.contains("other") && error.contains("state"), "{error}");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn the_oldest_delivery_ids_are_forgotten() {
    let mut seen = Seen::new(listen::REMEMBERED);
    assert!(seen.first("a"));
    assert!(!seen.first("a"));
    for n in 0..listen::REMEMBERED {
        assert!(seen.first(&n.to_string()));
    }
    assert!(seen.first("a"), "forgotten after REMEMBERED others");
    assert!(!seen.first(&(listen::REMEMBERED - 1).to_string()));
}

/// The ids remembered as the queue and set they were before the ring: the same answers over
/// seeded ids, with repeats, ids longer than a slot's room, and every size from one.
#[test]
fn the_ring_of_ids_answers_as_a_queue_and_a_set_did() {
    let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for most in [1, 2, 3, 7, 64, 500] {
        let mut seen = Seen::new(most);
        let mut order = std::collections::VecDeque::new();
        let mut ids = std::collections::HashSet::new();
        for step in 0..20 * most + 200 {
            let pick = rng.next_u64();
            let id = match pick % 3 {
                0 => format!("{:08x}-cc78-11e3-81ab-4c9367dc0958", pick % (3 * most as u64)),
                1 => format!("{}", pick % (2 * most as u64)),
                _ => "x".repeat(60 + (pick % 20) as usize),
            };
            let want = if ids.contains(&id) {
                false
            } else {
                if order.len() == most
                    && let Some(oldest) = order.pop_front()
                {
                    ids.remove(&oldest);
                }
                order.push_back(id.clone());
                ids.insert(id.clone());
                true
            };
            assert_eq!(seen.first(&id), want, "{most} {step} {id}");
        }
    }
}

/// A connection's bytes in, and what it was answered, in memory.
struct Wire<'a> {
    input: &'a [u8],
    at: usize,
    written: &'a mut Vec<u8>,
}

impl std::io::Read for Wire<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.input.len() - self.at);
        buf[..n].copy_from_slice(&self.input[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for Wire<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_warm_webhook_delivery_allocates_only_the_event_it_hands_on() {
    let path = temp("webhook-budget-secret");
    std::fs::write(&path, "It's a Secret to Everybody\n").unwrap();
    let secret = Arc::new(Secret::read(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    let (service, inputs) = crossbeam_channel::unbounded();
    let shared = Listener::remembering(secret.clone(), "o/r".to_owned(), service, 4);
    let mut parser = http::RequestParser::new(crate::webhook::MOST_PAYLOAD);
    let mut answer = listen::Answer::default();
    let mut written = Vec::with_capacity(4096);
    let wire = |kind: &str, id: &str, body: &str| {
        let signature = sign(secret.expose(), body.as_bytes());
        let headers = [
            ("X-GitHub-Event", kind),
            ("X-GitHub-Delivery", id),
            ("X-Hub-Signature-256", signature.as_str()),
            ("Content-Type", "application/json"),
        ];
        http::request("POST", "localhost", "/", &headers, body.as_bytes())
    };
    let mut exchange = |bytes: &[u8], written: &mut Vec<u8>| {
        written.clear();
        parser.reset();
        let mut stream = Wire { input: bytes, at: 0, written };
        listen::exchange(&mut stream, &shared, &mut parser, &mut answer);
    };
    let answered = |status: u16, said: &str| http::response(status, &listen_headers(), format!("{said}\n").as_bytes());

    for n in 0..8 {
        exchange(&wire("push", &format!("warm-{n}"), &push_body(n)), &mut written);
        assert_eq!(written, answered(202, "accepted"), "{n}");
    }
    let new = wire("push", "new", &push_body(8));
    let again = wire("push", "new", &push_body(8));
    let ping = wire("ping", "ping", r#"{"zen":"Keep it logically awesome.","hook_id":1}"#);
    let other = push_body(9).replace("o/r", "x/y");
    let elsewhere = wire("push", "elsewhere", &other);
    blaze_alloc_count::assert_at_most(
        3,
        "a new delivery: its event's branch and commit, handed to the service, and at most one \
         block of the service's channel, which holds 31",
        || exchange(&new, &mut written),
    );
    assert_eq!(written, answered(202, "accepted"));
    blaze_alloc_count::assert_at_most(0, "a redelivery", || exchange(&again, &mut written));
    assert_eq!(written, answered(202, "delivery new was already received"));
    blaze_alloc_count::assert_at_most(0, "a ping", || exchange(&ping, &mut written));
    assert_eq!(written, answered(202, "nothing for CI to run"));
    exchange(&elsewhere, &mut written);
    assert_eq!(written, answered(403, "a delivery for x/y, and this service is for o/r"));

    let events: Vec<Input> = inputs.try_iter().collect();
    assert_eq!(events.len(), 9);
    assert_eq!(
        events[8],
        Input::Event(Event::Push { branch: "b8".to_owned(), commit: format!("{:040x}", 9) })
    );
}

#[test]
fn a_workers_warm_reads_allocate_only_what_each_response_owns() {
    use blaze_api::{Envelope, Response, TestOutcome, TestResult, frames};

    let done = |id: u64| frames::encode(&Envelope::new(id, Response::Done { ok: id % 2 == 0 })).unwrap();
    let batch: Vec<u8> = (0..32).flat_map(|id| done(id)).collect();
    let mut incoming = frames::Frames::new();
    let mut seen: Vec<u64> = Vec::with_capacity(64);
    incoming.push(&batch);
    assert_eq!(super::worker::drain(&mut incoming, |envelope| { seen.push(envelope.id); true }), Ok(true));
    assert_eq!(seen, (0..32).collect::<Vec<u64>>());
    seen.clear();
    incoming.push(&batch);
    let read = blaze_alloc_count::assert_at_most(0, "32 `Done`s read in place", || {
        super::worker::drain(&mut incoming, |envelope| {
            seen.push(envelope.id);
            true
        })
    });
    assert_eq!((read, seen.len()), (Ok(true), 32));

    let result = TestResult {
        binary: "blaze_ci".to_owned(),
        name: "a_test".to_owned(),
        cached: None,
        outcome: TestOutcome::Passed,
        output: b"ok\n".to_vec(),
        millis: 3,
        outside: false,
        executable: 0,
    };
    incoming.push(&frames::encode(&Envelope::new(40, Response::TestResult(result.clone()))).unwrap());
    let mut kept = None;
    blaze_alloc_count::assert_at_most(3, "a test result: its binary, name and output, handed to the service", || {
        super::worker::drain(&mut incoming, |envelope| {
            kept = Some(envelope);
            true
        })
    });
    assert_eq!(kept.map(|it| (it.id, it.body)), Some((40, Response::TestResult(result))));

    incoming.push(&done(41));
    assert_eq!(super::worker::drain(&mut incoming, |_| false), Ok(false), "a worker gone stops the reader");
}

#[test]
fn a_workers_warm_requests_are_written_from_one_buffer() {
    use blaze_api::{Envelope, Request, frames};

    let mut out = super::worker::Outgoing::default();
    let mut sink: Vec<u8> = Vec::with_capacity(1 << 10);
    assert!(out.send(&mut sink, 1, Request::Cancel { id: 7 }).is_ok());
    assert_eq!(sink, frames::encode(&Envelope::new(1, Request::Cancel { id: 7 })).unwrap());
    sink.clear();
    let sent = blaze_alloc_count::assert_at_most(0, "a cancel sent", || {
        out.send(&mut sink, 2, Request::Cancel { id: 8 }).is_ok()
    });
    assert!(sent);
    assert_eq!(sink, frames::encode(&Envelope::new(2, Request::Cancel { id: 8 })).unwrap());
}

/// The headers every webhook answer has, as `http::response` was called with before.
fn listen_headers() -> [(&'static str, &'static str); 2] {
    [("Content-Type", "text/plain; charset=utf-8"), ("Connection", "close")]
}

/// Fails the first `failing` calls.
struct Flaky {
    failing: u32,
    calls: u32,
}

impl Checks for Flaky {
    fn create(&mut self, _: &CheckRun) -> Result<u64, GitHubError> {
        self.calls += 1;
        if self.calls <= self.failing {
            return Err(GitHubError { what: "502".to_owned(), status: Some(502) });
        }
        Ok(77)
    }

    fn update(&mut self, _: u64, run: &CheckRun) -> Result<(), GitHubError> {
        self.create(run).map(drop)
    }
}

fn run_check() -> CheckRun {
    CheckRun { name: "blaze/check".to_owned(), commit: "c".to_owned(), status: CheckStatus::Queued, summary: String::new() }
}

#[test]
fn github_errors_are_retried_five_times() {
    for (failing, created) in [(0, true), (5, true), (6, false)] {
        let github = Arc::new(Mutex::new(Flaky { failing, calls: 0 }));
        let (posts, receive) = crossbeam_channel::unbounded();
        let (service, inputs) = crossbeam_channel::unbounded();
        posts.send(Post::Create { run: RunId(3), check: run_check() }).unwrap();
        drop(posts);
        post(github.clone(), receive, service, Duration::ZERO);
        let Ok(Input::Created { run, result }) = inputs.recv() else { panic!("nothing created") };
        assert_eq!(run, RunId(3));
        assert_eq!(result.is_ok(), created, "{failing}: {result:?}");
        if let Err(what) = result {
            assert!(what.contains("blaze/check") && what.contains("6 times"), "{what}");
        }
        assert_eq!(github.lock().calls, (failing + 1).min(6));
    }
}

/// Records every call, and gives each creation the next id from 100.
#[derive(Default)]
struct Recorded {
    created: Vec<String>,
    updated: Vec<(u64, CheckStatus)>,
}

impl Checks for Recorded {
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError> {
        self.created.push(run.name.clone());
        Ok(100 + self.created.len() as u64)
    }

    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError> {
        self.updated.push((id, run.status));
        Ok(())
    }
}

fn update(run: u64, id: u64, status: CheckStatus) -> Post {
    Post::Update { run: RunId(run), id, check: CheckRun { status, ..run_check() } }
}

#[test]
fn an_update_with_a_later_one_of_its_run_waiting_is_never_sent() {
    let github = Arc::new(Mutex::new(Recorded::default()));
    let (posts, receive) = crossbeam_channel::unbounded();
    let (service, inputs) = crossbeam_channel::unbounded();
    posts.send(update(1, 11, CheckStatus::Queued)).unwrap();
    posts.send(update(1, 11, CheckStatus::InProgress)).unwrap();
    posts.send(Post::Create { run: RunId(2), check: run_check() }).unwrap();
    posts.send(update(3, 33, CheckStatus::InProgress)).unwrap();
    posts.send(update(1, 11, CheckStatus::Success)).unwrap();
    drop(posts);
    post(github.clone(), receive, service, Duration::ZERO);
    let recorded = github.lock();
    assert_eq!(recorded.created, ["blaze/check"]);
    assert_eq!(recorded.updated, [(33, CheckStatus::InProgress), (11, CheckStatus::Success)]);
    assert!(matches!(inputs.recv(), Ok(Input::Created { run: RunId(2), result: Ok(101) })));
}

#[test]
fn updates_of_different_runs_are_all_sent_in_order() {
    let github = Arc::new(Mutex::new(Recorded::default()));
    let (posts, receive) = crossbeam_channel::unbounded();
    let (service, _inputs) = crossbeam_channel::unbounded();
    for id in [5, 4, 6] {
        posts.send(update(id, id, CheckStatus::Failure)).unwrap();
    }
    drop(posts);
    post(github.clone(), receive, service, Duration::ZERO);
    let statuses = [(5, CheckStatus::Failure), (4, CheckStatus::Failure), (6, CheckStatus::Failure)];
    assert_eq!(github.lock().updated, statuses);
}

/// Takes `wait` per call, counting how many calls are in flight at once across every lane.
struct Slow {
    wait: Duration,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    most: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<Mutex<Vec<(u64, CheckStatus)>>>,
}

impl Checks for Slow {
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        blaze_spin::busy(self.wait);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let id: u64 = run.name.parse().unwrap();
        self.calls.lock().push((id, run.status));
        Ok(id)
    }

    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError> {
        self.create(&CheckRun { name: id.to_string(), ..run.clone() }).map(drop)
    }
}

/// Lanes as `serve` makes them, over fakes that share their counters.
fn slow_lanes(wait: Duration) -> (Posts, Arc<std::sync::atomic::AtomicUsize>, Arc<Mutex<Vec<(u64, CheckStatus)>>>, crossbeam_channel::Receiver<Input>) {
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (service, inputs) = crossbeam_channel::unbounded();
    let mut lanes = Vec::new();
    for _ in 0..super::POST_LANES {
        let (to_lane, posts) = crossbeam_channel::unbounded();
        let fake = Slow { wait, in_flight: in_flight.clone(), most: most.clone(), calls: calls.clone() };
        let service = service.clone();
        thread::spawn(move || post(Arc::new(Mutex::new(fake)), posts, service, Duration::ZERO));
        lanes.push(to_lane);
    }
    (Posts { lanes }, most, calls, inputs)
}

#[test]
fn the_runs_of_a_job_post_at_once_each_on_its_lane() {
    let (posts, most, calls, inputs) = slow_lanes(Duration::from_millis(50));
    let runs = super::POST_LANES as u64;
    for run in 0..runs {
        let check = CheckRun { name: run.to_string(), ..run_check() };
        posts.send(Post::Create { run: RunId(run), check });
    }
    let started = std::time::Instant::now();
    for _ in 0..runs {
        assert!(matches!(inputs.recv_timeout(Duration::from_secs(5)), Ok(Input::Created { result: Ok(_), .. })));
    }
    assert!(most.load(std::sync::atomic::Ordering::SeqCst) > 1, "every creation was sent alone");
    assert!(started.elapsed() < Duration::from_millis(50) * runs as u32, "{:?}", started.elapsed());
    assert_eq!(calls.lock().len(), runs as usize);
}

#[test]
fn a_runs_posts_stay_in_order_on_its_lane() {
    let (posts, _, calls, _inputs) = slow_lanes(Duration::from_millis(1));
    let statuses = [CheckStatus::Queued, CheckStatus::InProgress, CheckStatus::Failure];
    for status in statuses {
        for run in 0..3 * super::POST_LANES as u64 {
            posts.send(update(run, run, status));
        }
        let sent = (statuses.iter().position(|it| *it == status).unwrap() + 1) * 3 * super::POST_LANES;
        blaze_spin::SpinWait::within("the batch's posts", Duration::from_secs(60), || calls.lock().len() >= sent);
    }
    drop(posts);
    let calls = calls.lock();
    for run in 0..3 * super::POST_LANES as u64 {
        let seen: Vec<CheckStatus> = calls.iter().filter(|(id, _)| *id == run).map(|(_, status)| *status).collect();
        assert!(seen.windows(2).all(|pair| statuses.iter().position(|it| *it == pair[0]) < statuses.iter().position(|it| *it == pair[1])), "{run}: {seen:?}");
        assert_eq!(seen.last(), Some(&CheckStatus::Failure), "{run}: {seen:?}");
    }
}

#[test]
fn a_large_file_keeps_its_buffer_for_the_first_piece() {
    let bytes = vec![3u8; 2500];
    let pointer = bytes.as_ptr();
    let cut = batches(vec![Op::Write { path: "big".to_owned(), bytes, mode: Mode::Regular }], 1000).unwrap();
    let Some(Op::Write { bytes, .. }) = cut.into_iter().flatten().next() else { panic!("no write first") };
    assert_eq!(bytes.as_ptr(), pointer, "the first piece is the file's own buffer, not a copy");
}

fn sign(secret: &[u8], body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={hex}")
}

fn deliver(at: std::net::SocketAddr, id: &str, signature: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(at).unwrap();
    let headers = [
        ("X-GitHub-Event", "push"),
        ("X-GitHub-Delivery", id),
        ("X-Hub-Signature-256", signature),
        ("Content-Type", "application/json"),
    ];
    stream.write_all(&http::request("POST", "localhost", "/", &headers, body)).unwrap();
    let response = http::read_response(&mut stream, 1 << 20).unwrap();
    (response.status, String::from_utf8_lossy(&response.body).into_owned())
}

#[test]
fn needs_loopback_a_delivery_is_checked_answered_and_handed_on_once() {
    let path = temp("webhook-secret");
    std::fs::write(&path, "It's a Secret to Everybody\n").unwrap();
    let secret = Arc::new(Secret::read(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let (service, inputs) = crossbeam_channel::unbounded();
    let shared = Arc::new(Listener::new(secret.clone(), "o/r".to_owned(), service));
    thread::spawn(move || listen::serve(listener, shared));

    let commit = "0123456789abcdef0123456789abcdef01234567";
    let body = format!(
        r#"{{"ref":"refs/heads/main","after":"{commit}","repository":{{"full_name":"o/r"}}}}"#
    );
    let signature = sign(secret.expose(), body.as_bytes());
    assert_eq!(deliver(at, "d1", &signature, body.as_bytes()).0, 202);
    assert_eq!(
        inputs.recv_timeout(Duration::from_secs(5)).unwrap(),
        Input::Event(Event::Push { branch: "main".to_owned(), commit: commit.to_owned() })
    );
    let (status, said) = deliver(at, "d1", &signature, body.as_bytes());
    assert_eq!(status, 202);
    assert!(said.contains("already"), "{said}");

    let (status, _) = deliver(at, "d2", &sign(b"another secret", body.as_bytes()), body.as_bytes());
    assert_eq!(status, 401);

    let other = body.replace("o/r", "x/y");
    let (status, said) = deliver(at, "d3", &sign(secret.expose(), other.as_bytes()), other.as_bytes());
    assert_eq!(status, 403);
    assert!(said.contains("x/y"), "{said}");
    assert!(inputs.recv_timeout(Duration::from_millis(200)).is_err(), "only the first was handed on");
}

fn loopback_listener() -> (std::net::SocketAddr, Arc<Secret>, crossbeam_channel::Receiver<Input>, Arc<Listener>) {
    let path = temp(&format!("webhook-secret-{:?}", thread::current().id()));
    std::fs::write(&path, "It's a Secret to Everybody\n").unwrap();
    let secret = Arc::new(Secret::read(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let (service, inputs) = crossbeam_channel::unbounded();
    let shared = Arc::new(Listener::new(secret.clone(), "o/r".to_owned(), service));
    let serving = Arc::clone(&shared);
    thread::spawn(move || listen::serve(listener, serving));
    (at, secret, inputs, shared)
}

fn push_body(n: usize) -> String {
    format!(r#"{{"ref":"refs/heads/b{n}","after":"{:040x}","repository":{{"full_name":"o/r"}}}}"#, n + 1)
}

#[test]
fn needs_loopback_deliveries_at_once_are_each_answered_by_a_kept_handler() {
    let (at, secret, inputs, _) = loopback_listener();
    for round in 0..3 {
        let answered: Vec<u16> = thread::scope(|scope| {
            let sent: Vec<_> = (0..16)
                .map(|n| {
                    let body = push_body(round * 16 + n);
                    let signature = sign(secret.expose(), body.as_bytes());
                    let id = format!("d{round}-{n}");
                    scope.spawn(move || deliver(at, &id, &signature, body.as_bytes()).0)
                })
                .collect();
            sent.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(answered.iter().all(|status| *status == 202), "{answered:?}");
    }
    let mut branches: Vec<String> = (0..48)
        .map(|_| match inputs.recv_timeout(Duration::from_secs(5)).unwrap() {
            Input::Event(Event::Push { branch, .. }) => branch,
            other => panic!("{other:?}"),
        })
        .collect();
    branches.sort();
    let mut expected: Vec<String> = (0..48).map(|n| format!("b{n}")).collect();
    expected.sort();
    assert_eq!(branches, expected);
}

#[test]
fn needs_loopback_a_connection_over_the_most_open_is_answered_503() {
    let (at, secret, inputs, shared) = loopback_listener();
    let held: Vec<TcpStream> = (0..listen::MOST_CONNECTIONS).map(|_| TcpStream::connect(at).unwrap()).collect();
    let open = |count: usize, what: &str| {
        blaze_spin::SpinWait::within(what, Duration::from_secs(60), || shared.open() == count);
    };
    open(listen::MOST_CONNECTIONS, "the listener to take every held connection");
    let body = push_body(0);
    let (status, said) = deliver(at, "over", &sign(secret.expose(), body.as_bytes()), body.as_bytes());
    assert_eq!(status, 503, "{said}");
    drop(held);
    open(0, "the listener to let go of the held connections");
    assert_eq!(deliver(at, "after", &sign(secret.expose(), body.as_bytes()), body.as_bytes()).0, 202);
    assert!(matches!(inputs.recv_timeout(Duration::from_secs(5)), Ok(Input::Event(_))));
}

/// A post's retries wait the backoff, doubled after each failure, and the last wait is the
/// backoff times 2 to the retries less one.
#[test]
fn a_posts_waits_double_from_its_backoff() {
    let backoff = Duration::from_millis(250);
    let waits: Vec<_> = (0..GITHUB_RETRIES).map(|tried| waited(backoff, tried)).collect();
    assert_eq!(waits, [250, 500, 1000, 2000, 4000].map(Duration::from_millis));
    assert_eq!(waited(Duration::MAX, 1), Duration::MAX);
}
