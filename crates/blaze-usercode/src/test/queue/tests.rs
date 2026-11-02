use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    time::Duration,
};

use blaze_core::{
    Address, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName, IoError, Sandbox, Tag,
    Traced,
    sim::{self, Faults, Rng, Sim},
};
use blaze_ids::{ContentId, rng::SplitMix64};

use super::*;
use crate::{
    confine,
    test::wire::{self, End, Executed, Frame, Frames, SOCKET, UNRECORDED},
    trace::{Line, Seen},
};

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config(jobs: usize) -> Config {
    Config {
        root: PathBuf::from("/root"),
        dir: name("req/1"),
        jobs,
        fail_fast: false,
        kill_grace: Duration::from_secs(5),
        tags: 1 << 40,
    }
}

fn job(name: &str, expected_ms: u64) -> Job {
    Job {
        name: name.into(),
        ignore: false,
        expected: Duration::from_millis(expected_ms),
        timeout: Duration::from_secs(60),
        retries: 0,
    }
}

fn binary(name: &str, jobs: Vec<Job>) -> Binary {
    Binary {
        id: ContentId::of("blaze v2 test binary 1", name.as_bytes()),
        name: name.into(),
        program: PathBuf::from(format!("/bin/{name}")),
        env: vec![("PATH".into(), "/usr/bin".into())],
        dir: "/ws".into(),
        read: vec!["/ws".into()],
        write: vec!["/tmp/t".into()],
        network: false,
        memory: None,
        mode: Mode::Process,
        jobs,
        loaded: None,
        sandboxed: true,
    }
}

/// `binary` outside the sandbox.
fn outside(name: &str, jobs: Vec<Job>) -> Binary {
    Binary { sandboxed: false, ..binary(name, jobs) }
}

fn not_known() -> Trace {
    Trace::Unknown(OUTSIDE.to_owned())
}

fn exited(code: Option<i32>, signal: Option<i32>, traced: Option<Traced>) -> Done {
    Done::Exited(Box::new(Exit {
        code,
        signal,
        stdout: b"server out".to_vec(),
        stderr: Vec::new(),
        traced,
        peak_memory: 0,
    }))
}

fn lines(it: &[(Seen, &str)]) -> Trace {
    Trace::Lines(it.iter().copied().collect())
}

fn unrecorded() -> Executed {
    Executed::Not(UNRECORDED.to_owned())
}

/// The hello indices of `runs`.
fn indices(runs: &[(u64, u32)]) -> Vec<u32> {
    runs.iter().map(|it| it.1).collect()
}

/// A driver played by hand: it answers at once what needs no decision (writes, reads of the
/// records, sends, hang-ups, unlistens), and keeps the rest for the test to answer. Its clock
/// moves only by [`Fake::advance`], which fires the timers due on the way.
struct Fake {
    queue: Queue,
    now: Duration,
    /// Each timer set, when it fires, and how many sends had been asked for before it.
    timers: Vec<(Tag, Duration, usize)>,
    spawns: Vec<(Tag, Command)>,
    kills: Vec<Tag>,
    listens: Vec<(Tag, Address)>,
    recvs: Vec<(Tag, Conn)>,
    sent: Vec<(Conn, Frame)>,
    /// How many sends the queue asked for.
    sends: usize,
    /// The connections a send to breaks (`Done::Broken`), as one whose peer is gone does.
    breaks: Vec<Conn>,
    hangups: Vec<Conn>,
    records: BTreeMap<FileName, Vec<u8>>,
    /// The record reads the queue asked for.
    reads: Vec<FileName>,
    next_conn: u64,
    results: Vec<Finished>,
    /// What the queue allocated in its `complete` calls, and nothing of the fake's.
    allocations: u64,
}

impl Fake {
    fn new(config: Config, binaries: Vec<Binary>) -> Fake {
        let mut effects = Vec::new();
        let queue = Queue::new(config, binaries, Duration::ZERO, &mut effects);
        let mut fake = Fake {
            queue,
            now: Duration::ZERO,
            timers: Vec::new(),
            spawns: Vec::new(),
            kills: Vec::new(),
            listens: Vec::new(),
            recvs: Vec::new(),
            sent: Vec::new(),
            sends: 0,
            breaks: Vec::new(),
            hangups: Vec::new(),
            records: BTreeMap::new(),
            reads: Vec::new(),
            next_conn: 0,
            results: Vec::new(),
            allocations: 0,
        };
        fake.pump(effects);
        fake
    }

    fn complete(&mut self, tag: Tag, done: Done) {
        assert!(self.queue.owns(tag));
        let mut effects = Vec::with_capacity(64);
        self.queue_complete(Completion { tag, done }, &mut effects);
        self.pump(effects);
    }

    fn queue_complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) {
        let (queue, now) = (&mut self.queue, self.now);
        let ((), used) = blaze_alloc_count::measure(|| queue.complete(completion, now, effects));
        self.allocations += used.allocations;
    }

    fn cancel(&mut self) {
        let mut effects = Vec::new();
        self.queue.cancel(self.now, &mut effects);
        self.pump(effects);
    }

    fn pump(&mut self, mut effects: Vec<Effect>) {
        while !effects.is_empty() {
            let mut answers = Vec::new();
            for effect in effects.drain(..) {
                match effect {
                    Effect::Write { file, bytes, tag } => {
                        self.records.insert(file, bytes);
                        answers.push(Completion { tag, done: Done::Written });
                    }
                    Effect::Read { file, at, len, tag } => {
                        self.reads.push(file.clone());
                        let all = self.records.get(&file).cloned().unwrap_or_default();
                        let at = (at as usize).min(all.len());
                        let bytes = all[at..(at + len).min(all.len())].to_vec();
                        answers.push(Completion { tag, done: Done::Read(bytes) });
                    }
                    Effect::Send { conn, mut bytes, tag } if self.breaks.contains(&conn) => {
                        self.sends += 1;
                        bytes.clear();
                        let error = IoError("Broken pipe (os error 32)".into());
                        answers.push(Completion { tag, done: Done::Broken { buffer: bytes, error } });
                    }
                    Effect::Send { conn, mut bytes, tag } => {
                        let mut frames = Frames::new();
                        frames.push(&bytes);
                        while let Some(frame) = frames.pop().unwrap() {
                            self.sent.push((conn, frame));
                        }
                        self.sends += 1;
                        bytes.clear();
                        answers.push(Completion { tag, done: Done::Sent(bytes) });
                    }
                    Effect::Unlisten { tag } => {
                        self.listens.retain(|it| it.0 != tag);
                        answers.push(Completion { tag, done: Done::Unlistened });
                    }
                    Effect::Hangup { conn, tag } => {
                        self.hangups.push(conn);
                        answers.push(Completion { tag, done: Done::HungUp });
                    }
                    Effect::Timer { after, tag } => self.timers.push((tag, self.now + after, self.sends)),
                    Effect::Listen { at, tag } => self.listens.push((tag, at)),
                    Effect::Recv { conn, tag, .. } => self.recvs.push((tag, conn)),
                    Effect::Spawn { command, tag } => self.spawns.push((tag, *command)),
                    Effect::Kill { tag } => self.kills.push(tag),
                    other => panic!("the queue asked for {other:?}"),
                }
            }
            for answer in answers {
                self.queue_complete(answer, &mut effects);
            }
        }
        self.results.extend(self.queue.drain());
    }

    /// The clock moved on by `by`, each timer due by then fired at its time, the earliest
    /// first.
    fn advance(&mut self, by: Duration) {
        let to = self.now + by;
        loop {
            let Some(at) = (0..self.timers.len()).filter(|&at| self.timers[at].1 <= to).min_by_key(|&at| self.timers[at].1) else {
                break;
            };
            let (tag, when, _) = self.timers.remove(at);
            self.now = self.now.max(when);
            self.complete(tag, Done::Fired);
        }
        self.now = to;
    }

    fn accept(&mut self, b: usize) -> Conn {
        let at = Address::Unix(name(&format!("req/1/t{b}")));
        let tag = self.listens.iter().find(|it| it.1 == at).expect("a listen").0;
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.complete(tag, Done::Accepted { conn });
        conn
    }

    fn say_bytes(&mut self, conn: Conn, bytes: Vec<u8>) {
        let at = self.recvs.iter().position(|it| it.1 == conn).expect("a receive on the connection");
        let (tag, _) = self.recvs.remove(at);
        self.complete(tag, Done::Read(bytes));
    }

    fn say(&mut self, conn: Conn, frame: Frame) {
        self.say_bytes(conn, frame.to_bytes());
    }

    /// A server that measured its fork as free, so it stays one.
    fn hello(&mut self, b: usize, tests: &[&str]) -> Conn {
        self.hello_with(b, tests, 0, 1)
    }

    /// A connection of `b`'s server accepted and its hello said: a fork of `fork` nanoseconds,
    /// process `pid`.
    fn hello_with(&mut self, b: usize, tests: &[&str], fork: u64, pid: u32) -> Conn {
        let conn = self.accept(b);
        self.say_hello(conn, tests, fork, pid);
        conn
    }

    fn say_hello(&mut self, conn: Conn, tests: &[&str], fork: u64, pid: u32) {
        let tests = tests.iter().map(|it| (*it).to_owned()).collect();
        self.say(conn, Frame::Hello { version: wire::VERSION, pid, fork, tests });
    }

    /// How many replicas `conn` was asked for since the last call; the rest of what it was
    /// sent is kept.
    fn replicate(&mut self, conn: Conn) -> Option<u32> {
        let at = self.sent.iter().position(|it| it.0 == conn && matches!(it.1, Frame::Replicate { .. }))?;
        match self.sent.remove(at).1 {
            Frame::Replicate { count } => Some(count),
            _ => None,
        }
    }

    fn end(&mut self, conn: Conn, id: u64, end: End, output: &[u8]) {
        self.end_with(conn, id, end, output, unrecorded());
    }

    fn end_with(&mut self, conn: Conn, id: u64, end: End, output: &[u8], executed: Executed) {
        self.say(conn, Frame::Ended { id, end, output: output.to_vec(), executed });
    }

    /// The frames sent to `conn` since the last call.
    fn frames(&mut self, conn: Conn) -> Vec<Frame> {
        let (to, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sent).into_iter().partition(|it| it.0 == conn);
        self.sent = rest;
        to.into_iter().map(|it| it.1).collect()
    }

    fn runs(&mut self, conn: Conn) -> Vec<(u64, u32)> {
        self.frames(conn)
            .into_iter()
            .filter_map(|it| match it {
                Frame::Run { id, index } => Some((id, index)),
                _ => None,
            })
            .collect()
    }

    /// The one run sent to `conn` since the last call, which must be of hello index `index`.
    fn run(&mut self, conn: Conn, index: u32) -> u64 {
        let runs = self.runs(conn);
        assert_eq!(indices(&runs), [index], "{runs:?}");
        runs[0].0
    }

    fn append(&mut self, b: usize, lines: &[Line]) {
        let text: String = lines.iter().map(Line::text).collect();
        self.records.get_mut(&name(&format!("req/1/r{b}"))).expect("the record").extend_from_slice(text.as_bytes());
    }

    fn spawn_of(&self, program: &str) -> Tag {
        self.spawns.iter().find(|it| it.1.program == PathBuf::from(program)).expect("a spawn").0
    }

    fn exit(&mut self, program: &str, done: Done) {
        let tag = self.spawn_of(program);
        self.spawns.retain(|it| it.0 != tag);
        self.complete(tag, done);
    }

    fn result(&self, test: &str) -> &Finished {
        self.results.iter().find(|it| it.test.as_deref() == Some(test)).unwrap_or_else(|| panic!("no result of {test}: {:?}", self.results))
    }
}

#[test]
fn tests_run_from_the_fork_server_longest_first_with_their_own_reads() {
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 10), job("b", 50), job("c", 30)])]);
    assert_eq!(fake.records.get(&name("req/1/r0")), Some(&Vec::new()));
    let command = &fake.spawns[0].1;
    assert!(command.env.contains(&(SOCKET.into(), "/root/req/1/t0".into())));
    assert!(command.env.contains(&(confine::LIVE.into(), "/root/req/1/r0".into())));
    assert!(command.env.contains(&("PATH".into(), "/usr/bin".into())));
    assert!(command.args.is_empty());
    let Sandbox::On { read, write, network, trace, .. } = &command.sandbox else { panic!("{command:?}") };
    assert!(*trace && !*network);
    assert!(read.contains(&PathBuf::from("/root/req/1/t0")) && read.contains(&PathBuf::from("/ws")));
    assert_eq!(write, &vec![PathBuf::from("/tmp/t")]);
    let conn = fake.hello(0, &["a", "b", "c"]);
    // Two cores: b and c, the longest.
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [1, 2]);
    let (b, c) = (runs[0].0, runs[1].0);
    // One driver timer, for the earlier deadline.
    assert_eq!(fake.timers.len(), 1);
    fake.append(0, &[
        Line::new(Seen::Read, "", "/lib/libc.so"),
        Line::new(Seen::Env, "", SOCKET),
        Line::new(Seen::Looked, "", "/root/req/1/t0"),
        Line::new(Seen::Read, "b", "/ws/b.txt"),
        Line::new(Seen::Read, "c", "/ws/c.txt"),
    ]);
    fake.advance(Duration::from_millis(40));
    fake.end(conn, b, End::Exited(0), b"b ok");
    let result = fake.result("b");
    assert_eq!((&result.outcome, result.output.as_slice(), result.attempts), (&Outcome::Passed, &b"b ok"[..], 1));
    assert_eq!(result.took, Duration::from_millis(40));
    assert_eq!(result.trace, lines(&[(Seen::Read, "/lib/libc.so"), (Seen::Read, "/ws/b.txt")]));
    assert_eq!(result.executed, unrecorded());
    let Trace::Lines(b_read) = &result.trace else { panic!("{result:?}") };
    assert_eq!(b_read.common(), 1);
    // A core is free: a runs.
    let a = fake.run(conn, 0);
    fake.append(0, &[Line::new(Seen::Env, "c", "HOME")]);
    fake.end(conn, c, End::Exited(101), b"c failed");
    let result = fake.result("c");
    assert_eq!(result.outcome, Outcome::Failed(Failure::Code(101)));
    let Trace::Lines(read) = &result.trace else { panic!("{result:?}") };
    let read: Vec<(Seen, &str)> = read.iter().collect();
    assert!(read.contains(&(Seen::Env, "HOME")) && read.contains(&(Seen::Read, "/ws/c.txt")), "{read:?}");
    // No line of every scope came between b's result and c's: they share one buffer of them.
    let (Trace::Lines(b_read), Trace::Lines(c_read)) = (&fake.result("b").trace, &fake.result("c").trace) else { panic!() };
    assert!(c_read.common_is(b_read) && c_read.common() == 1);
    assert!(fake.frames(conn).is_empty());
    // One comes before a's result: a's has a new buffer of both.
    fake.append(0, &[Line::new(Seen::Read, "", "/lib/libm.so")]);
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.result("a").outcome, Outcome::Passed);
    let (Trace::Lines(a_read), Trace::Lines(c_read)) = (&fake.result("a").trace, &fake.result("c").trace) else { panic!() };
    assert!(!a_read.common_is(c_read) && a_read.common() == 2);
    assert_eq!(a_read.iter().take(2).collect::<Vec<_>>(), [(Seen::Read, "/lib/libc.so"), (Seen::Read, "/lib/libm.so")]);
    assert_eq!(c_read.iter().take(1).collect::<Vec<_>>(), [(Seen::Read, "/lib/libc.so")]);
    // Nothing is left: the server is told to quit, and the queue is done once it has ended.
    assert_eq!(fake.frames(conn), vec![Frame::Quit]);
    assert!(!fake.queue.finished());
    fake.exit("/bin/lib", exited(Some(0), None, Some(Traced::default())));
    assert!(fake.queue.finished());
    assert_eq!(fake.results.len(), 3);
    // The deadlines went with their tests: the timer still in flight does nothing.
    fake.advance(Duration::from_secs(120));
    assert!(fake.frames(conn).is_empty() && fake.kills.is_empty());
}

#[test]
fn a_test_past_its_timeout_is_killed_and_a_server_that_does_not_kill_it_is_killed() {
    let mut slow = job("slow", 0);
    slow.timeout = Duration::from_secs(1);
    let mut stuck = job("stuck", 0);
    stuck.timeout = Duration::from_secs(2);
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![slow, stuck, job("later", 0)])]);
    let conn = fake.hello(0, &["slow", "stuck", "later"]);
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [0, 1]);
    let (slow, stuck) = (runs[0].0, runs[1].0);
    // Never early.
    fake.advance(Duration::from_millis(999));
    assert!(fake.frames(conn).is_empty());
    fake.advance(Duration::from_millis(1));
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: slow }]);
    fake.end(conn, slow, End::Signaled(9), b"partial");
    assert_eq!(fake.result("slow").outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(1))));
    let later = fake.run(conn, 2);
    fake.end(conn, later, End::Exited(0), b"");
    fake.advance(Duration::from_secs(1));
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: stuck }]);
    fake.advance(Duration::from_millis(4_999));
    assert!(fake.kills.is_empty());
    fake.advance(Duration::from_millis(1));
    let server = fake.spawn_of("/bin/lib");
    assert_eq!(fake.kills, vec![server]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("stuck").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("did not end") && why.contains("lib"), "{why}");
    assert!(fake.result("stuck").output.ends_with(b"server out"));
    assert_eq!(fake.result("stuck").executed, Executed::Not(why.clone()));
    assert!(fake.queue.finished());
}

#[test]
fn only_tests_named_flaky_run_again_and_report_every_attempt() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 2;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky, job("solid", 0)])]);
    let conn = fake.hello(0, &["flaky", "solid"]);
    let first = fake.run(conn, 0);
    fake.end(conn, first, End::Exited(1), b"first\n");
    assert!(fake.results.is_empty());
    let second = fake.run(conn, 0);
    assert_ne!(first, second, "a retry is a new attempt, though its slot is reused");
    fake.end(conn, second, End::Exited(0), b"second\n");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Passed, 2));
    assert_eq!(flaky.output, b"--- attempt 1 ---\nfirst\n--- attempt 2 ---\nsecond\n");
    let solid = fake.run(conn, 1);
    fake.end(conn, solid, End::Exited(1), b"");
    let solid = fake.result("solid");
    assert_eq!((&solid.outcome, solid.attempts), (&Outcome::Failed(Failure::Code(1)), 1));
}

#[test]
fn a_flaky_test_that_fails_every_time_fails_after_its_retries() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 1;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky, job("solid", 0)])]);
    let conn = fake.hello(0, &["flaky", "solid"]);
    let first = fake.run(conn, 0);
    fake.end(conn, first, End::Signaled(6), b"");
    let second = fake.run(conn, 0);
    fake.end(conn, second, End::Exited(1), b"");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Failed(Failure::Code(1)), 2));
    let solid = fake.run(conn, 1);
    fake.end(conn, solid, End::Exited(1), b"");
    let solid = fake.result("solid");
    assert_eq!((&solid.outcome, solid.attempts), (&Outcome::Failed(Failure::Code(1)), 1));
}

#[test]
fn fail_fast_stops_at_the_first_failure_and_no_fail_fast_runs_all() {
    for fail_fast in [true, false] {
        let config = Config { fail_fast, ..config(1) };
        let mut fake = Fake::new(config, vec![binary("lib", vec![job("a", 30), job("b", 20), job("c", 10)])]);
        let conn = fake.hello(0, &["a", "b", "c"]);
        let a = fake.run(conn, 0);
        fake.end(conn, a, End::Exited(1), b"");
        assert_eq!(fake.result("a").outcome, Outcome::Failed(Failure::Code(1)));
        if fail_fast {
            assert_eq!(fake.result("b").outcome, Outcome::NotRun);
            assert_eq!(fake.result("c").outcome, Outcome::NotRun);
            assert!(matches!(&fake.result("c").executed, Executed::Not(why) if why.contains("stopped")));
            assert_eq!(fake.frames(conn), vec![Frame::Quit]);
        } else {
            let b = fake.run(conn, 1);
            fake.end(conn, b, End::Exited(0), b"");
            let c = fake.run(conn, 2);
            fake.end(conn, c, End::Exited(0), b"");
            assert_eq!(fake.frames(conn), vec![Frame::Quit]);
        }
        fake.exit("/bin/lib", exited(Some(0), None, None));
        assert!(fake.queue.finished());
        assert_eq!(fake.results.len(), 3);
    }
}

#[test]
fn one_queue_over_every_binary_runs_the_longest_ready_test_first() {
    let mut fake = Fake::new(
        config(1),
        vec![binary("a", vec![job("big", 100), job("tiny", 1)]), binary("b", vec![job("mid", 50), job("small", 20)])],
    );
    let a = fake.hello(0, &["big", "tiny"]);
    let big = fake.run(a, 0);
    let b = fake.hello(1, &["small", "mid"]);
    assert!(fake.runs(b).is_empty());
    fake.end(a, big, End::Exited(0), b"");
    assert!(fake.runs(a).is_empty());
    // mid is index 1 of b's hello.
    let mid = fake.run(b, 1);
    fake.end(b, mid, End::Exited(0), b"");
    let small = fake.run(b, 0);
    fake.end(b, small, End::Exited(0), b"");
    assert_eq!(fake.frames(b), vec![Frame::Quit]);
    let tiny = fake.run(a, 1);
    fake.end(a, tiny, End::Exited(0), b"");
    assert_eq!(fake.frames(a), vec![Frame::Quit]);
    let order: Vec<&str> = fake.results.iter().map(|it| it.test.as_deref().unwrap()).collect();
    assert_eq!(order, ["big", "mid", "small", "tiny"]);
    assert_eq!((fake.results[1].binary, fake.results[3].binary), (1, 0));
}

/// Units of equal expected time keep their binary-then-job order, as the sorted set of
/// `(Reverse(expected), binary, job)` that the order replaced had them.
#[test]
fn ties_in_the_order_go_by_binary_then_job() {
    let mut fake = Fake::new(
        config(1),
        vec![binary("a", vec![job("x", 5), job("y", 9), job("z", 5)]), binary("b", vec![job("w", 5), job("v", 9)])],
    );
    let order: Vec<(u32, u32)> =
        fake.queue.order.iter().map(|&unit| (fake.queue.unit_bin[unit as usize], fake.queue.unit_job[unit as usize])).collect();
    assert_eq!(order, [(0, 1), (1, 1), (0, 0), (0, 2), (1, 0)]);
    for (at, &unit) in fake.queue.order.iter().enumerate() {
        assert_eq!(fake.queue.rank[unit as usize], at as u32);
    }
    let a = fake.hello(0, &["x", "y", "z"]);
    let b = fake.hello(1, &["w", "v"]);
    let y = fake.run(a, 1);
    fake.end(a, y, End::Exited(0), b"");
    let v = fake.run(b, 1);
    fake.end(b, v, End::Exited(0), b"");
    let x = fake.run(a, 0);
    fake.end(a, x, End::Exited(0), b"");
}

#[test]
fn a_binary_that_cannot_start_loses_its_tests_with_why() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let spawn = fake.spawn_of("/bin/lib");
    fake.spawns.clear();
    fake.complete(spawn, Done::Failed(IoError("no user namespaces".into())));
    for test in ["a", "b"] {
        let Outcome::Failed(Failure::Lost(why)) = &fake.result(test).outcome else { panic!("{:?}", fake.results) };
        assert!(why.contains("could not be started in its sandbox: no user namespaces"), "{why}");
        assert_eq!(fake.result(test).trace, Trace::Unknown(why.clone()));
        assert_eq!(fake.result(test).executed, Executed::Not(why.clone()));
    }
    assert!(fake.listens.is_empty());
    assert!(fake.spawns.is_empty(), "nothing is started outside in its place: {:?}", fake.spawns);
    assert!(fake.queue.finished());
}

/// Outside the sandbox the server is spawned at once, with no sandbox, no record and no
/// `LIVE`; its tests run from it as inside, pass and fail as inside, and what each read is
/// not known, so selection keeps nothing of it.
#[test]
fn a_binary_outside_the_sandbox_runs_its_tests_with_no_record() {
    let mut fake = Fake::new(config(3), vec![outside("lib", vec![job("a", 30), job("b", 20), job("c", 10)])]);
    assert!(fake.records.is_empty(), "no record is made: {:?}", fake.records);
    let command = &fake.spawns[0].1;
    assert_eq!(command.sandbox, Sandbox::Off);
    assert!(command.env.contains(&(SOCKET.into(), "/root/req/1/t0".into())));
    assert!(!command.env.iter().any(|it| it.0 == confine::LIVE), "{:?}", command.env);
    assert!(command.env.contains(&("PATH".into(), "/usr/bin".into())));
    let conn = fake.hello(0, &["a", "b", "c"]);
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [0, 1, 2]);
    let hits = || Executed::Hits { slots: 8, hits: vec![1] };
    fake.end_with(conn, runs[0].0, End::Exited(0), b"a ok", hits());
    fake.end(conn, runs[1].0, End::Exited(101), b"b failed");
    fake.end(conn, runs[2].0, End::Signaled(6), b"");
    assert_eq!((&fake.result("a").outcome, fake.result("a").output.as_slice()), (&Outcome::Passed, &b"a ok"[..]));
    assert_eq!(fake.result("a").executed, hits());
    assert_eq!(fake.result("b").outcome, Outcome::Failed(Failure::Code(101)));
    assert_eq!(fake.result("c").outcome, Outcome::Failed(Failure::Signal(6)));
    for test in ["a", "b", "c"] {
        assert_eq!(fake.result(test).trace, not_known(), "{test}");
    }
    let mut inputs = crate::inputs::Disk::new(Vec::new());
    assert!(fake.result("a").trace.traced(&mut inputs).unwrap().is_none());
    assert!(fake.reads.is_empty() && fake.records.is_empty());
    assert_eq!(fake.frames(conn), vec![Frame::Quit]);
    fake.exit("/bin/lib", exited(Some(0), None, None));
    assert!(fake.queue.finished());
}

/// A failed or retried test outside is judged as inside; one that can't be started says so
/// without naming a sandbox.
#[test]
fn a_test_outside_is_retried_and_lost_as_inside() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 1;
    let mut fake = Fake::new(config(1), vec![outside("lib", vec![flaky])]);
    let conn = fake.hello(0, &["flaky"]);
    let first = fake.run(conn, 0);
    fake.end(conn, first, End::Exited(1), b"first\n");
    let second = fake.run(conn, 0);
    fake.end(conn, second, End::Exited(0), b"second\n");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts, &flaky.trace), (&Outcome::Passed, 2, &not_known()));

    let mut fake = Fake::new(config(1), vec![outside("lib", vec![job("a", 0)])]);
    let spawn = fake.spawn_of("/bin/lib");
    fake.spawns.clear();
    fake.complete(spawn, Done::Failed(IoError("no such file".into())));
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("a").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("/bin/lib could not be started: no such file"), "{why}");
    assert!(fake.queue.finished());
}

/// A package's `unsandboxed-tests`: two binaries of one program, the sandboxed run with the
/// rest of its tests and the outside run with those named. Each has its own socket and runs
/// only its own tests; the outside one starts once the sandboxed tests have their results.
#[test]
fn an_outside_and_a_sandboxed_run_of_one_program_each_run_their_own_tests() {
    let inside = binary("lib", vec![job("a", 30), job("b", 20)]);
    let out = Binary { jobs: vec![job("writes_home", 50)], ..outside("lib", Vec::new()) };
    assert_eq!((&inside.id, &inside.program), (&out.id, &out.program));
    let mut fake = Fake::new(config(4), vec![inside, out]);
    assert_eq!(fake.spawns.len(), 1, "the outside run waits: {:?}", fake.spawns);
    assert!(matches!(fake.spawns[0].1.sandbox, Sandbox::On { trace: true, .. }));
    assert_eq!(fake.records.keys().collect::<Vec<_>>(), [&name("req/1/r0")]);
    let all = ["a", "b", "writes_home"];
    let inside = fake.hello(0, &all);
    let runs = fake.runs(inside);
    assert_eq!(indices(&runs), [0, 1], "the outside test is not the sandboxed run's");
    fake.append(0, &[Line::new(Seen::Read, "a", "/ws/a")]);
    fake.end(inside, runs[0].0, End::Exited(0), b"");
    assert_eq!(fake.spawns.len(), 1, "a sandboxed test has no result yet");
    fake.end(inside, runs[1].0, End::Exited(0), b"");
    assert_eq!(fake.result("a").trace, lines(&[(Seen::Read, "/ws/a")]));
    assert_eq!((fake.result("a").binary, fake.result("b").binary), (0, 0));
    assert_eq!(fake.frames(inside), vec![Frame::Quit]);
    // Every sandboxed test has its result: the outside run starts, on its own socket.
    assert_eq!(fake.spawns.len(), 2);
    let command = &fake.spawns[1].1;
    assert_eq!((command.sandbox.clone(), command.program.clone()), (Sandbox::Off, PathBuf::from("/bin/lib")));
    assert!(command.env.contains(&(SOCKET.into(), "/root/req/1/t1".into())));
    assert!(!command.env.iter().any(|it| it.0 == confine::LIVE));
    assert!(!fake.records.contains_key(&name("req/1/r1")));
    let out = fake.hello(1, &all);
    let home = fake.run(out, 2);
    fake.end(out, home, End::Exited(0), b"");
    let result = fake.result("writes_home");
    assert_eq!((result.binary, &result.outcome, &result.trace), (1, &Outcome::Passed, &not_known()));
    assert!(fake.reads.iter().all(|it| *it == name("req/1/r0")), "{:?}", fake.reads);
    assert_eq!(fake.frames(out), vec![Frame::Quit]);
    let (first, second) = (fake.spawns[0].0, fake.spawns[1].0);
    fake.complete(first, exited(Some(0), None, Some(Traced::default())));
    assert!(!fake.queue.finished());
    fake.complete(second, exited(Some(0), None, None));
    assert!(fake.queue.finished());
    assert_eq!(fake.results.len(), 3);
}

/// A stop before the sandboxed tests are done leaves the outside run unstarted, its tests
/// not run.
#[test]
fn fail_fast_inside_leaves_the_outside_run_unstarted() {
    let config = Config { fail_fast: true, ..config(1) };
    let out = Binary { jobs: vec![job("x", 0)], ..outside("lib", Vec::new()) };
    let mut fake = Fake::new(config, vec![binary("lib", vec![job("a", 0)]), out]);
    let inside = fake.hello(0, &["a", "x"]);
    let a = fake.run(inside, 0);
    fake.end(inside, a, End::Exited(1), b"");
    assert_eq!(fake.result("a").outcome, Outcome::Failed(Failure::Code(1)));
    assert_eq!(fake.result("x").outcome, Outcome::NotRun);
    assert_eq!(fake.spawns.len(), 1, "{:?}", fake.spawns);
    assert!(fake.listens.iter().all(|it| it.1 != Address::Unix(name("req/1/t1"))));
    let first = fake.spawns[0].0;
    fake.complete(first, exited(Some(0), None, None));
    assert!(fake.queue.finished());
}

/// A package that runs whole as libtest's threads, outside: no sandbox, and its trace is not
/// known whatever its exit says.
#[test]
fn a_binary_run_whole_outside_has_no_sandbox() {
    let mut old = outside("old", vec![job("x", 5)]);
    old.mode = Mode::Threads { args: Vec::new(), timeout: Duration::from_secs(10) };
    let mut fake = Fake::new(config(4), vec![old]);
    assert!(fake.records.is_empty() && fake.listens.is_empty());
    let command = &fake.spawns[0].1;
    assert_eq!(command.sandbox, Sandbox::Off);
    assert!(!command.env.iter().any(|it| it.0 == confine::LIVE || it.0 == SOCKET));
    fake.exit("/bin/old", exited(Some(0), None, Some(Traced::default())));
    let whole = &fake.results[0];
    assert_eq!((&whole.outcome, &whole.trace), (&Outcome::Passed, &not_known()));
    assert!(fake.queue.finished());
}

#[test]
fn a_test_the_binary_does_not_have_is_lost() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 0), job("gone", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("gone").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("no test named gone"), "{why}");
    fake.run(conn, 0);
}

#[test]
fn a_server_that_breaks_the_protocol_or_hangs_up_is_killed() {
    for case in 0..3 {
        let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
        let conn = fake.hello(0, &["a", "b"]);
        assert_eq!(fake.runs(conn).len(), 2);
        let expected = match case {
            0 => {
                fake.say_bytes(conn, vec![1, 0, 0, 0, 77]);
                "broke the protocol"
            }
            1 => {
                fake.say(conn, Frame::Run { id: 0, index: 0 });
                "broke the protocol"
            }
            _ => {
                fake.say_bytes(conn, Vec::new());
                "closed its socket"
            }
        };
        let server = fake.spawn_of("/bin/lib");
        assert_eq!(fake.kills, vec![server]);
        fake.exit("/bin/lib", exited(None, Some(9), None));
        for test in ["a", "b"] {
            let Outcome::Failed(Failure::Lost(why)) = &fake.result(test).outcome else { panic!("{:?}", fake.results) };
            assert!(why.contains(expected), "{why}");
        }
        assert!(fake.queue.finished());
    }
}

#[test]
fn an_end_of_a_test_it_does_not_run_is_a_broken_protocol() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let a = fake.run(conn, 0);
    fake.end(conn, a + 5, End::Exited(0), b"");
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
}

/// An attempt's id outlives it: once its slot is another's, an end of the old id is of no
/// test the server runs. Outside the sandbox, so the slot is free before the next test is sent.
#[test]
fn an_end_of_an_attempt_already_ended_is_a_broken_protocol() {
    let mut fake = Fake::new(config(1), vec![outside("lib", vec![job("a", 1), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.end(conn, a, End::Exited(0), b"");
    let b = fake.run(conn, 1);
    assert_eq!(a as u32, b as u32, "the slot is reused");
    assert_ne!(a, b);
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    assert!(matches!(fake.result("b").outcome, Outcome::Failed(Failure::Lost(_))));
}

#[test]
fn a_record_that_is_not_one_leaves_what_was_read_unknown() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"Q\tnot\ta line\n");
    fake.end(conn, a, End::Exited(0), b"");
    let result = fake.result("a");
    assert_eq!(result.outcome, Outcome::Passed);
    assert!(matches!(&result.trace, Trace::Unknown(why) if why.contains("req/1/r0")));
    let b = fake.run(conn, 1);
    fake.end(conn, b, End::Exited(0), b"");
    assert!(matches!(&fake.result("b").trace, Trace::Unknown(_)));
}

#[test]
fn a_line_being_written_waits_for_the_next_read() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/a\nR\tb\t/ws/");
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.result("a").trace, lines(&[(Seen::Read, "/ws/a")]));
    let b = fake.run(conn, 1);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"b\n");
    fake.end(conn, b, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, lines(&[(Seen::Read, "/ws/b")]));
}

#[test]
fn a_package_that_opts_out_runs_whole_as_libtest_s_threads() {
    let old = || {
        let mut ignored = job("y", 5);
        ignored.ignore = true;
        let mut old = binary("old", vec![job("x", 5), ignored]);
        old.mode = Mode::Threads { args: vec!["--nocapture".into()], timeout: Duration::from_secs(10) };
        old
    };
    let mut fake = Fake::new(config(4), vec![old()]);
    assert!(fake.records.is_empty() && fake.listens.is_empty());
    let command = &fake.spawns[0].1;
    let args: Vec<String> = command.args.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    assert_eq!(args, ["x", "y", "--exact", "--test-threads=2", "--include-ignored", "--nocapture"]);
    assert!(!command.env.iter().any(|it| it.0 == SOCKET));
    assert!(matches!(command.sandbox, Sandbox::On { trace: true, .. }));
    let traced = Traced { env: vec![("HOME".into(), Some("/h".into()))], ..Traced::default() };
    fake.exit("/bin/old", exited(Some(0), None, Some(traced.clone())));
    let whole = &fake.results[0];
    assert_eq!((whole.test.as_ref(), &whole.outcome, &whole.trace), (None, &Outcome::Passed, &Trace::Whole(traced)));
    assert_eq!(whole.executed, Executed::Not(WHOLE_UNRECORDED.to_owned()));
    assert!(fake.queue.finished());

    let mut fake = Fake::new(config(4), vec![old()]);
    fake.advance(Duration::from_millis(9_999));
    assert!(fake.kills.is_empty());
    fake.advance(Duration::from_millis(1));
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/old")]);
    fake.exit("/bin/old", exited(None, Some(sim::KILLED), None));
    assert_eq!(fake.results[0].outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(10))));
    assert!(matches!(&fake.results[0].trace, Trace::Unknown(_)));
}

#[test]
fn a_cancel_kills_what_runs_and_runs_nothing_more() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 10), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    fake.run(conn, 0);
    fake.cancel();
    assert_eq!(fake.result("b").outcome, Outcome::NotRun);
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    assert_eq!(fake.result("a").outcome, Outcome::NotRun);
    assert!(fake.runs(conn).is_empty());
    assert!(fake.queue.finished());
}

/// What the fork server read of a test's region comes with its result, as it was sent.
#[test]
fn what_a_test_executed_comes_with_its_result() {
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 2), job("b", 1)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let runs = fake.runs(conn);
    let hits = || Executed::Hits { slots: 64, hits: vec![3, 9, 63] };
    fake.end_with(conn, runs[0].0, End::Exited(0), b"", hits());
    fake.end_with(conn, runs[1].0, End::Exited(1), b"", Executed::Not("its region was not armed".to_owned()));
    assert_eq!(fake.result("a").executed, hits());
    assert_eq!(fake.result("b").executed, Executed::Not("its region was not armed".to_owned()));
}

/// The fork server of a simulated binary, played by the test from outside the simulation:
/// it says hello, notes each test's reads in the record and answers each run.
struct Played {
    tests: Vec<String>,
    conn: Option<Conn>,
    frames: Frames,
    quit: bool,
}

/// Two binaries on two cores under the simulation's reordering and latency, on many seeds:
/// every test ends once, with what it read and executed, and the queue ends with every
/// process. With the second outside the sandbox, its tests' reads are not known and it
/// starts after the first's tests have their results.
#[test]
fn the_queue_under_a_simulated_driver() {
    for two_outside in [false, true] {
        simulated(two_outside);
    }
}

fn simulated(two_outside: bool) {
    for seed in 0..64 {
        let mut sim = Sim::new(seed, Faults::default());
        for program in ["/bin/one", "/bin/two"] {
            sim.program(program, |_: &Command, _: &mut Rng| sim::Ran {
                exit: Exit { code: Some(0), signal: None, stdout: Vec::new(), stderr: Vec::new(), traced: Some(Traced::default()), peak_memory: 0 },
                takes: Duration::from_secs(3600),
            });
        }
        let mut one = binary("one", vec![job("a", 30), job("fails", 20), job("c", 10)]);
        one.jobs[1].retries = 0;
        let two = Binary { sandboxed: !two_outside, ..binary("two", vec![job("d", 50), job("e", 5)]) };
        let mut effects = Vec::new();
        let mut queue = Queue::new(config(2), vec![one, two], sim.now(), &mut effects);
        let mut played: Vec<Played> = [vec!["a", "fails", "c"], vec!["d", "e"]]
            .into_iter()
            .map(|tests| Played { tests: tests.into_iter().map(str::to_owned).collect(), conn: None, frames: Frames::new(), quit: false })
            .collect();
        let mut results = Vec::new();
        let mut out = Vec::new();
        let mut rounds = 0;
        while !queue.finished() {
            rounds += 1;
            assert!(rounds < 100_000, "seed {seed}: the queue never ended");
            sim.submit(&mut effects);
            // The servers act on what was submitted before time moves: a poll goes to the
            // soonest event, which can be a server's exit an hour on, before it dialed.
            for (b, server) in played.iter_mut().enumerate() {
                let socket = Address::Unix(name(&format!("req/1/t{b}")));
                let Some(conn) = server.conn.or_else(|| sim.dial(&socket).ok()) else { continue };
                if server.conn.is_none() {
                    server.conn = Some(conn);
                    let hello = Frame::Hello { version: wire::VERSION, pid: 7, fork: 0, tests: server.tests.clone() };
                    sim.peer_send(conn, &hello.to_bytes());
                }
                server.frames.push(&sim.peer_received(conn));
                while let Some(frame) = server.frames.pop().unwrap() {
                    match frame {
                        Frame::Run { id, index } => {
                            let test = &server.tests[index as usize];
                            if two_outside && b == 1 {
                                assert!(results.len() >= 3, "seed {seed}: two ran before one's tests had results");
                            } else {
                                let record = name(&format!("req/1/r{b}"));
                                let mut bytes = sim.contents(&record);
                                bytes.extend_from_slice(Line::new(Seen::Read, test, &format!("/ws/{test}")).text().as_bytes());
                                sim.change(&record, &bytes);
                            }
                            let end = match test.as_str() {
                                "fails" => End::Exited(101),
                                _ => End::Exited(0),
                            };
                            let executed = Executed::Hits { slots: 16, hits: vec![index] };
                            let ended = Frame::Ended { id, end, output: test.as_bytes().to_vec(), executed };
                            sim.peer_send(conn, &ended.to_bytes());
                        }
                        Frame::Quit => {
                            server.quit = true;
                            sim.peer_hangup(conn);
                        }
                        other => panic!("seed {seed}: blaze sent {other:?}"),
                    }
                }
            }
            sim.poll(&mut out, Duration::ZERO);
            for completion in out.drain(..) {
                assert!(queue.owns(completion.tag), "seed {seed}");
                queue.complete(completion, sim.now(), &mut effects);
            }
            results.extend(queue.drain());
        }
        let quit: Vec<bool> = played.iter().map(|it| it.quit).collect();
        assert!(quit.iter().all(|it| *it), "seed {seed}, two outside {two_outside}: quit {quit:?}");
        let mut names: Vec<&str> = results.iter().map(|it| it.test.as_deref().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a", "c", "d", "e", "fails"], "seed {seed}");
        for result in &results {
            let test = result.test.as_deref().unwrap();
            let expected = match test {
                "fails" => Outcome::Failed(Failure::Code(101)),
                _ => Outcome::Passed,
            };
            assert_eq!(result.outcome, expected, "seed {seed}");
            assert_eq!(result.output, test.as_bytes(), "seed {seed}");
            let trace = match two_outside && result.binary == 1 {
                true => not_known(),
                false => lines(&[(Seen::Read, &format!("/ws/{test}"))]),
            };
            assert_eq!(result.trace, trace, "seed {seed}");
            let index = played[result.binary].tests.iter().position(|it| it == test).unwrap() as u32;
            assert_eq!(result.executed, Executed::Hits { slots: 16, hits: vec![index] }, "seed {seed}");
        }
    }
}

#[test]
fn what_one_call_says_to_a_server_goes_as_one_send_with_the_timer_after_it() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 4), job("b", 3), job("c", 2), job("d", 1)])]);
    let before = fake.sends;
    let conn = fake.hello(0, &["a", "b", "c", "d"]);
    assert_eq!(fake.sends, before + 1);
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [0, 1, 2, 3]);
    // One driver timer for the four deadlines, asked for after the send.
    assert_eq!(fake.timers.len(), 1);
    assert_eq!(fake.timers[0].2, before + 1);
    assert!(fake.timers[0].1 <= Duration::from_secs(60));
    fake.advance(Duration::from_millis(59_999));
    assert!(fake.frames(conn).is_empty());
    fake.advance(Duration::from_millis(1));
    let mut killed: Vec<u64> = fake
        .frames(conn)
        .into_iter()
        .map(|it| match it {
            Frame::Kill { id } => id,
            other => panic!("{other:?}"),
        })
        .collect();
    killed.sort_unstable();
    let mut ran: Vec<u64> = runs.iter().map(|it| it.0).collect();
    ran.sort_unstable();
    assert_eq!(killed, ran);
    assert_eq!(fake.sends, before + 2, "the four kills of one call go as one send");
}

#[test]
fn a_line_of_every_scope_is_given_once_however_often_it_comes() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    let libc = Line::new(Seen::Read, "", "/lib/libc.so");
    let looked = Line::new(Seen::Looked, "", "/lib/libc.so");
    fake.append(0, &[libc.clone(), libc.clone(), looked.clone(), Line::new(Seen::Read, "a", "/ws/a"), libc.clone()]);
    fake.end(conn, a, End::Exited(0), b"");
    let common = [(Seen::Read, "/lib/libc.so"), (Seen::Looked, "/lib/libc.so")];
    assert_eq!(fake.result("a").trace, lines(&[common[0], common[1], (Seen::Read, "/ws/a")]));
    let b = fake.run(conn, 1);
    fake.append(0, &[libc, Line::new(Seen::Read, "b", "/ws/b")]);
    fake.end(conn, b, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, lines(&[common[0], common[1], (Seen::Read, "/ws/b")]));
}

#[test]
fn a_line_that_is_not_utf_8_is_read_as_the_lossy_record_has_it() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/\xff\xfex\n");
    fake.end(conn, a, End::Exited(0), b"");
    let text = String::from_utf8_lossy(b"R\ta\t/ws/\xff\xfex").into_owned();
    let line = Line::parse(&text).unwrap();
    assert_eq!(fake.result("a").trace, lines(&[(line.seen, &line.what)]));
}

/// The lines a test is given go to [`trace::assemble`] as the lines it was: scope dropped,
/// which `assemble` reads as of every scope.
#[test]
fn lines_are_read_back_as_they_were_pushed() {
    let given = [(Seen::Read, "/a"), (Seen::Env, "HOME"), (Seen::Read, ""), (Seen::Listed, "/é/b")];
    let lines: Lines = given.iter().copied().collect();
    assert_eq!(lines.len(), 4);
    assert!(!lines.is_empty() && Lines::new().is_empty());
    assert_eq!(lines.iter().collect::<Vec<_>>(), given);
}

/// The queue's borrowed reading of a record's line against [`Line::parse`], over seeded lines
/// of the characters that matter (letters, tabs, a scope, a multi-byte character).
#[test]
fn a_line_is_read_in_place_as_line_parse_reads_it() {
    let alphabet = ['R', 'L', 'D', 'E', 'T', 'W', 'N', 'X', 'Q', '\t', 'a', '/', 'é', '*'];
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let len = rng.index(12);
        let text: String = (0..len).map(|_| alphabet[rng.index(alphabet.len())]).collect();
        let read = record_line(&text).map(|(seen, scope, what)| Line { seen, scope: scope.to_owned(), what: what.to_owned() });
        assert_eq!(read, Line::parse(&text), "{text:?}");
    }
}

/// [`SEEN`] is [`Seen`] in its declared order, so a packed line reads back its kind.
#[test]
fn a_packed_line_reads_back_its_kind() {
    for (at, &seen) in SEEN.iter().enumerate() {
        assert_eq!(seen as usize, at);
        assert_eq!(unpack(pack(names::MOST - 1, seen)), (names::MOST - 1, seen));
        assert_eq!(Seen::of(&Line::new(seen, "", "").text()[..1]), Some(seen));
        assert_eq!(record_line(&format!("{}\t\tx", &Line::new(seen, "", "").text()[..1])), Some((seen, "", "x")));
    }
}

/// The rank sets against a sorted set, over seeded sets, clears and firsts: the cursor never
/// skips a member set below it.
#[test]
fn ranks_are_a_set_with_a_cursor() {
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for len in [0usize, 1, 63, 64, 65, 200] {
        let mut bits = Ranks::full(len);
        let mut set: BTreeSet<u32> = (0..len as u32).collect();
        assert_eq!(bits.ones().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        if len == 0 {
            assert_eq!(bits.first(), None);
            continue;
        }
        for _ in 0..2_000 {
            let at = rng.below(len as u64) as u32;
            match rng.below(3) {
                0 => {
                    bits.set(at);
                    set.insert(at);
                }
                1 => {
                    bits.clear(at);
                    set.remove(&at);
                }
                _ => {
                    let first = bits.first();
                    assert_eq!(first, set.first().copied());
                    if let Some(first) = first {
                        bits.clear(first);
                        set.remove(&first);
                    }
                }
            }
            assert_eq!(bits.has(at), set.contains(&at));
        }
        assert_eq!(bits.ones().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        bits.clear_all();
        assert_eq!((bits.first(), bits.ones().next()), (None, None));
        let mut empty = Ranks::empty(len);
        assert_eq!(empty.first(), None);
    }
}

/// Many tests through few cores: the attempt slab, the tag slab, the wheel and each server's
/// lists stay at what the cores need, however many tests run.
#[test]
fn the_steady_state_reuses_its_slabs() {
    // One server, and four replicas (a fork of 10 s against a median test of 300 ms); in the
    // sandbox and outside it.
    for sandboxed in [true, false] {
        for fork in [0, 10_000_000_000] {
            steady_state(fork, sandboxed);
        }
    }
}

fn steady_state(fork: u64, sandboxed: bool) {
    let jobs: Vec<Job> = (0..600).map(|n| job(&format!("t{n}"), 600 - n)).collect();
    let names: Vec<String> = jobs.iter().map(|it| it.name.clone()).collect();
    let plan = match sandboxed {
        true => binary("lib", jobs),
        false => outside("lib", jobs),
    };
    let mut fake = Fake::new(config(4), vec![plan]);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let first = fake.hello_with(0, &names, fork, 1);
    let mut conns = vec![first];
    for pid in 0..fake.replicate(first).unwrap_or(0) {
        conns.push(fake.hello_with(0, &names, fork, 2 + pid));
    }
    assert_eq!(conns.len(), if fork == 0 { 1 } else { 4 });
    let mut ended = 0;
    let mut high = None;
    while ended < 600 {
        let runs: Vec<(Conn, u64, u32)> =
            conns.clone().into_iter().flat_map(|conn| fake.runs(conn).into_iter().map(move |(id, index)| (conn, id, index))).collect();
        assert!(!runs.is_empty(), "nothing runs after {ended}");
        for (conn, id, index) in runs {
            if sandboxed {
                fake.append(0, &[Line::new(Seen::Read, "", "/lib/libc.so"), Line::new(Seen::Read, names[index as usize], "/ws/x")]);
            }
            let before = fake.allocations;
            fake.advance(Duration::from_millis(3));
            fake.end(conn, id, End::Exited(0), b"ok");
            ended += 1;
            // What a test costs the queue once its columns have grown: its result's output,
            // execution record and name, its own line (kept, then its result's two buffers),
            // the driver's record read (it owns the file name) and the send of the next run
            // (it owns the bytes). Nothing of the lines every test read, nothing per timer.
            // Outside, no line and no read, and the reason its trace is not known.
            const PER_TEST: u64 = 8;
            let used = fake.allocations - before;
            assert!(ended < 100 || used <= PER_TEST, "test {ended}: {used} allocations, budget {PER_TEST}");
        }
        let queue = &fake.queue;
        let server = queue.bins[0].server.as_ref().unwrap();
        let now = (queue.attempts.len(), queue.tags.len(), server.waiting.capacity(), server.next.capacity());
        if ended == 100 {
            high = Some(now);
        }
        // Once the last test has ended the queue tells every replica to quit, which is no test's.
        if let Some(high) = high
            && ended < 600
        {
            assert_eq!(now, high, "after {ended} tests");
        }
        // A sandboxed test holds its slot until its record is read, after the next one is sent.
        assert!(queue.attempts.len() <= 4 + usize::from(sandboxed) && queue.wheel.len() <= 8, "{now:?}");
    }
    assert_eq!(fake.results.len(), 600);
    assert!(fake.results.iter().all(|it| it.outcome == Outcome::Passed));
    let server = fake.queue.bins[0].server.as_ref().unwrap();
    if !sandboxed {
        assert!(fake.reads.is_empty() && fake.records.is_empty());
        assert!(fake.results.iter().all(|it| it.trace == not_known()));
        assert_eq!((server.common.len(), server.names.len()), (0, 4));
        return;
    }
    // The line of every scope is kept once; every name once (blaze's four, then two).
    assert_eq!(server.common.len(), 1);
    assert_eq!(server.names.len(), 6);
}

/// `K = ceil(C × F / (F + T))`, at least one, at most the cores and the tests.
#[test]
fn replicas_follow_the_cores_the_fork_and_the_median_test() {
    // Tests that take nothing: one replica per core.
    assert_eq!(replicas(64, 50_000, 0, 10_000), 64);
    // A median test as long as 63 forks: 64 cores need one replica.
    assert_eq!(replicas(64, 50_000, 63 * 50_000, 10_000), 1);
    // 16 × 50 / 150 = 5.3.
    assert_eq!(replicas(16, 50_000, 100_000, 10_000), 6);
    assert_eq!(replicas(16, 50_000, 1_000_000, 10_000), 1);
    // A fork that costs nothing, fewer tests than cores, no cores said.
    assert_eq!(replicas(16, 0, 0, 10_000), 1);
    assert_eq!(replicas(16, 50_000, 0, 3), 3);
    assert_eq!(replicas(0, 50_000, 0, 0), 1);
    assert_eq!(replicas(usize::MAX, u64::MAX, 0, usize::MAX), u32::MAX);
    assert_eq!((median(&mut [5, 1, 3]), median(&mut []), median(&mut [7])), (3, 0, 7));
}

fn names_of(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("t{n}")).collect()
}

/// The first hello asks for the replicas before any run; once every replica said hello the
/// listen ends, and tests go to whichever replica has room (the cores over the replicas),
/// each in the one order.
#[test]
fn tests_are_spread_across_replicas() {
    let names = names_of(12);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(4), vec![binary("lib", names.iter().map(|it| job(it, 0)).collect())]);
    let first = fake.hello_with(0, &names, 1_000_000, 1);
    let sent = fake.frames(first);
    assert_eq!(sent[0], Frame::Replicate { count: 3 }, "asked first: {sent:?}");
    // Alone, it has room for every core.
    let mut flight: VecDeque<(Conn, u64, u32)> = VecDeque::new();
    for frame in &sent[1..] {
        let Frame::Run { id, index } = frame else { panic!("{sent:?}") };
        flight.push_back((first, *id, *index));
    }
    assert_eq!(flight.iter().map(|it| it.2).collect::<Vec<_>>(), [0, 1, 2, 3]);
    let mut conns = vec![first];
    for pid in 2..5 {
        conns.push(fake.hello_with(0, &names, 1_000_000, pid));
    }
    assert!(fake.listens.is_empty(), "every replica is in: the listen ended");
    assert_eq!(fake.queue.bins[0].server.as_ref().unwrap().room, 1);
    let mut ran: BTreeMap<u64, usize> = BTreeMap::new();
    let (mut order, mut quit) = (Vec::new(), Vec::new());
    while let Some((conn, id, index)) = flight.pop_front() {
        fake.end(conn, id, End::Exited(0), b"");
        for &conn in &conns {
            for frame in fake.frames(conn) {
                match frame {
                    Frame::Run { id, index } => {
                        *ran.entry(conn.0).or_default() += 1;
                        order.push(index);
                        flight.push_back((conn, id, index));
                    }
                    Frame::Quit => quit.push(conn),
                    other => panic!("{other:?}"),
                }
            }
        }
        // Past the first four, no replica runs more than its room.
        if index >= 3 {
            for &conn in &conns {
                assert!(flight.iter().filter(|it| it.0 == conn).count() <= 1, "{flight:?}");
            }
        }
    }
    assert_eq!(order, (4..12).collect::<Vec<u32>>(), "the one order, whichever replica");
    assert_eq!(ran.len(), 4, "every replica ran tests: {ran:?}");
    assert_eq!(fake.results.len(), 12);
    assert!(fake.results.iter().all(|it| it.outcome == Outcome::Passed));
    quit.sort_unstable();
    assert_eq!(quit, conns, "every replica is told to quit");
    fake.exit("/bin/lib", exited(Some(0), None, None));
    assert!(fake.queue.finished());
}

/// A replica that closes its socket loses only the test it ran, saying which replica and why;
/// the server is not killed, and the others run the rest with the room it left.
#[test]
fn a_replica_that_dies_mid_test_fails_only_its_own_test() {
    let names = names_of(6);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(3), vec![binary("lib", names.iter().map(|it| job(it, 0)).collect())]);
    let first = fake.hello_with(0, &names, 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(2));
    let runs = fake.runs(first);
    assert_eq!(indices(&runs), [0, 1, 2]);
    let (second, third) = (fake.hello_with(0, &names, 1_000_000, 2), fake.hello_with(0, &names, 1_000_000, 3));
    // Each end frees a core for the next replica with room: the server runs two still.
    fake.end(first, runs[0].0, End::Exited(0), b"");
    let on_second = fake.run(second, 3);
    fake.end(first, runs[1].0, End::Exited(0), b"");
    let on_third = fake.run(third, 4);
    fake.end(first, runs[2].0, End::Exited(0), b"");
    let on_first = fake.run(first, 5);
    // The second dies with t3 running.
    fake.say_bytes(second, Vec::new());
    let lost = fake.result("t3");
    let Outcome::Failed(Failure::Lost(why)) = &lost.outcome else { panic!("{lost:?}") };
    assert!(why.contains("lib: replica 1 of its fork server (pid 2) closed its socket"), "{why}");
    assert_eq!((&lost.trace, &lost.executed, lost.attempts), (&Trace::Unknown(why.clone()), &Executed::Not(why.clone()), 1));
    assert!(fake.kills.is_empty(), "the server and its other replicas carry on");
    let server = fake.queue.bins[0].server.as_ref().unwrap();
    assert_eq!((server.live, server.room), (2, 2));
    assert!(fake.frames(second).is_empty(), "nothing more is sent to it");
    assert!(!fake.recvs.iter().any(|it| it.1 == second), "nor read from it");
    fake.end(third, on_third, End::Exited(0), b"");
    fake.end(first, on_first, End::Exited(0), b"");
    let _ = on_second;
    for test in ["t0", "t1", "t2", "t4", "t5"] {
        assert_eq!(fake.result(test).outcome, Outcome::Passed, "{test}");
    }
    assert_eq!(fake.results.len(), 6);
    assert_eq!(fake.frames(first), vec![Frame::Quit]);
    assert_eq!(fake.frames(third), vec![Frame::Quit]);
    fake.exit("/bin/lib", exited(Some(0), None, None));
    assert!(fake.queue.finished());
}

/// A send that breaks loses the replica it was for, as a hangup does: its test is lost with why,
/// nothing more goes to it, and the server's other replicas carry on.
#[test]
fn a_replica_a_send_to_breaks_is_lost() {
    let names = names_of(4);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(2), vec![binary("lib", names.iter().map(|it| job(it, 0)).collect())]);
    let first = fake.hello_with(0, &names, 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(1));
    let runs = fake.runs(first);
    let second = fake.hello_with(0, &names, 1_000_000, 2);
    fake.breaks.push(second);
    fake.end(first, runs[0].0, End::Exited(0), b"");
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("t2").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("replica 1") && why.contains("could not be sent to"), "{why}");
    assert!(fake.kills.is_empty(), "the server carries on");
    assert!(fake.frames(second).is_empty(), "nothing reached it");
    fake.end(first, runs[1].0, End::Exited(0), b"");
    let t3 = fake.run(first, 3);
    fake.end(first, t3, End::Exited(0), b"");
    assert_eq!(fake.results.len(), 4);
    assert_eq!(fake.frames(first), vec![Frame::Quit]);
}

/// A replica's ends are its own: one sent on another replica's connection breaks the protocol
/// there, losing only what that replica runs.
#[test]
fn an_end_on_another_replica_s_connection_loses_only_that_replica() {
    let names = names_of(4);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(2), vec![binary("lib", names.iter().map(|it| job(it, 0)).collect())]);
    let first = fake.hello_with(0, &names, 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(1));
    let runs = fake.runs(first);
    let second = fake.hello_with(0, &names, 1_000_000, 2);
    fake.end(first, runs[0].0, End::Exited(0), b"");
    let on_second = fake.run(second, 2);
    fake.end(second, runs[1].0, End::Exited(0), b"");
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("t2").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("replica 1") && why.contains("broke the protocol"), "{why}");
    assert!(fake.hangups.contains(&second) && fake.kills.is_empty());
    let _ = on_second;
    // The server still runs t1, and t3 after it.
    fake.end(first, runs[1].0, End::Exited(0), b"");
    let t3 = fake.run(first, 3);
    fake.end(first, t3, End::Exited(0), b"");
    assert_eq!(fake.results.len(), 4);
    assert_eq!(fake.frames(first), vec![Frame::Quit]);
}

/// Until a replica says hello it is sent nothing; once it has, it takes its share. One whose
/// hello comes after the binary has nothing left is told to quit at once.
#[test]
fn a_replica_s_hello_may_come_late() {
    let names = names_of(4);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(2), vec![binary("lib", names.iter().map(|it| job(it, 0)).collect())]);
    let first = fake.hello_with(0, &names, 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(1));
    let runs = fake.runs(first);
    assert_eq!(indices(&runs), [0, 1]);
    let late = fake.accept(0);
    fake.end(first, runs[0].0, End::Exited(0), b"");
    // Connected but silent: the server runs the next.
    let t2 = fake.run(first, 2);
    assert!(fake.frames(late).is_empty());
    fake.say_hello(late, &names, 1_000_000, 2);
    assert!(fake.frames(late).is_empty(), "every core is busy");
    fake.end(first, runs[1].0, End::Exited(0), b"");
    let t3 = fake.run(late, 3);
    fake.end(late, t3, End::Exited(0), b"");
    fake.end(first, t2, End::Exited(0), b"");
    assert_eq!(fake.results.len(), 4);
    assert_eq!((fake.frames(first), fake.frames(late)), (vec![Frame::Quit], vec![Frame::Quit]));

    let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let first = fake.hello_with(0, &["a", "b"], 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(1));
    for (id, _) in fake.runs(first) {
        fake.end(first, id, End::Exited(0), b"");
    }
    assert_eq!(fake.frames(first), vec![Frame::Quit]);
    let late = fake.hello_with(0, &["a", "b"], 1_000_000, 2);
    assert_eq!(fake.frames(late), vec![Frame::Quit]);
    fake.say_bytes(late, Vec::new());
    fake.say_bytes(first, Vec::new());
    fake.exit("/bin/lib", exited(Some(0), None, None));
    assert!(fake.queue.finished());
    assert!(fake.results.iter().all(|it| it.outcome == Outcome::Passed));
}

/// A replica's hello must name the first's tests, and a replica can't ask again.
#[test]
fn a_replica_with_other_tests_or_a_second_hello_is_lost() {
    let mut fake = Fake::new(config(3), vec![binary("lib", vec![job("a", 0), job("b", 0), job("c", 0)])]);
    let first = fake.hello_with(0, &["a", "b", "c"], 1_000_000, 1);
    assert_eq!(fake.replicate(first), Some(2));
    let other = fake.hello_with(0, &["a", "b"], 1_000_000, 2);
    assert!(fake.hangups.contains(&other));
    let again = fake.hello_with(0, &["a", "b", "c"], 1_000_000, 3);
    fake.say_hello(again, &["a", "b", "c"], 1_000_000, 3);
    assert!(fake.hangups.contains(&again));
    assert!(fake.kills.is_empty() && fake.results.is_empty(), "the server runs a, b and c: {:?}", fake.results);
    assert_eq!(fake.runs(first).len(), 3);
}

/// The same tests give the same results in the same order whether one server or four replicas
/// run them: the order, the retries in their place, the reads and the outputs.
#[test]
fn replicas_give_the_results_of_one_server() {
    let one = drive(0);
    let four = drive(1_000_000_000);
    assert_eq!((one.1, four.1), (1, 4), "replicas that ran tests");
    assert_eq!(one.0.len(), 10);
    assert_eq!(one.0, four.0);
    let flaky = one.0.iter().find(|it| it.test.as_deref() == Some("flaky")).unwrap();
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Passed, 2));
    assert!(one.0.iter().any(|it| it.test.as_deref() == Some("bad") && it.outcome == Outcome::Failed(Failure::Code(1))));
}

/// Ten tests through four cores, each ended in the order it was sent, with a fork of `fork`
/// nanoseconds: the results, and how many connections ran tests.
fn drive(fork: u64) -> (Vec<Finished>, usize) {
    let mut jobs: Vec<Job> = (0..8).map(|n| job(&format!("t{n}"), 90 - 10 * n)).collect();
    let mut flaky = job("flaky", 55);
    flaky.retries = 1;
    jobs.push(flaky);
    jobs.push(job("bad", 45));
    let names: Vec<String> = jobs.iter().map(|it| it.name.clone()).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fake = Fake::new(config(4), vec![binary("lib", jobs)]);
    let first = fake.hello_with(0, &names, fork, 1);
    let mut conns = vec![first];
    let count = fake.replicate(first).unwrap_or(0);
    let mut flight: VecDeque<(Conn, u64, u32)> = fake.runs(first).into_iter().map(|(id, index)| (first, id, index)).collect();
    for pid in 0..count {
        conns.push(fake.hello_with(0, &names, fork, 2 + pid));
    }
    let mut used = BTreeSet::new();
    let mut tries: BTreeMap<u32, u32> = BTreeMap::new();
    while let Some((conn, id, index)) = flight.pop_front() {
        used.insert(conn.0);
        let name = names[index as usize];
        fake.append(0, &[Line::new(Seen::Read, "", "/lib/libc.so"), Line::new(Seen::Read, name, &format!("/ws/{name}"))]);
        fake.advance(Duration::from_millis(1));
        let tried = tries.entry(index).or_default();
        *tried += 1;
        let end = match (name, *tried) {
            ("bad", _) | ("flaky", 1) => End::Exited(1),
            _ => End::Exited(0),
        };
        fake.end(conn, id, end, name.as_bytes());
        for &conn in &conns {
            flight.extend(fake.runs(conn).into_iter().map(|(id, index)| (conn, id, index)));
        }
    }
    (fake.results, used.len())
}
