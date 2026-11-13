//! A warm `run` of the thin `blaze` against the simulation's daemon, end to end: the thin
//! client's own talk ([`blaze_thin::talk::talk`]) on a link that is a connection to the
//! simulation's socket, its rows sent last as `main.rs` sends them, the launcher's `Hello`
//! and end played into the launch host's join, and the rows the history is given read back.

use std::{ffi::OsString, path::Path};

use arrow_array::{Array as _, cast::AsArray as _, types::UInt32Type};
use blaze_thin::{
    answer::Shown,
    route::{self, Route},
    talk::{self, Ended, Link, Read, Spoken, Timings},
};

use super::{
    super::{daemon::DROPS_A_CLIENT_SPAN, observe::CLIENT_ROWS, unit::run::launch},
    *,
};

/// The process the thin client is, and its program becomes.
const PID: u32 = 4141;

/// Every span a warm run's build has, each exactly once: the thin client's five and the
/// launched program's two.
const SPANS: [&str; 7] = [
    talk::EXEC_TO_MAIN,
    talk::SOLO_CHECK,
    talk::CONNECT,
    talk::ON_THE_WIRE,
    blaze_api::Timed::ANSWER_TO_EXEC,
    launch::EXEC_TO_HELLO,
    launch::HELLO_TO_EXIT,
];

/// The thin client's one connection, to the simulation's daemon: each wait for a response
/// turns the daemon, at most [`TURNS`] times.
struct SimLink<'a> {
    daemon: &'a mut SimDaemon,
    conn: Conn,
    frames: Frames,
    read: std::collections::VecDeque<Envelope<Response>>, // alloc: a test's responses
    clock: Cell<i64>,
}

impl SimLink<'_> {
    fn take(&mut self) -> Result<(), String> {
        self.frames.push(&self.daemon.driver().peer_received(self.conn));
        while let Some(item) = self.frames.next().map_err(|error| format!("{error:?}"))? {
            let envelope: Envelope<Response> = frames::decode(&item).map_err(|error| format!("{error:?}"))?;
            self.read.push_back(envelope);
        }
        Ok(())
    }
}

impl Link for SimLink<'_> {
    type Until = ();

    fn within(&self, _: Duration) {}

    fn send(&mut self, id: u64, request: &Request) -> Result<Vec<u8>, String> {
        let bytes = frames::encode(&Envelope { schema: SCHEMA, id, body: request })
            .map_err(|error| format!("{error:?}"))?;
        self.daemon.driver().peer_send(self.conn, &bytes);
        Ok(bytes)
    }

    fn next(&mut self, (): (), _: &'static str) -> Result<Read, String> {
        for _ in 0..TURNS {
            self.take()?;
            if let Some(envelope) = self.read.pop_front() {
                return Ok(Read::Response(envelope));
            }
            self.daemon.turn();
        }
        Ok(Read::Deadline)
    }

    /// The simulation's sends are whole frames, and each read is taken whole into `read`.
    fn held(&self) -> usize {
        self.read.len()
    }

    fn forget(&mut self) {
        self.read.clear();
    }

    /// A test's clock, read only here: 10 a read, so each span has a length.
    fn now_ns(&self) -> i64 {
        self.clock.set(self.clock.get() + 10);
        self.clock.get()
    }
}

/// The rows the history was given for `build`, by span name: how many of each, and the
/// client times of its rows that carry them.
fn spans_of_build(build: u64) -> (BTreeMap<String, usize>, Vec<(i64, i64)>) {
    let mut named = BTreeMap::new();
    let mut times = Vec::new();
    // clone: a test's read of what the history was given
    for (of, timed, bytes) in CLIENT_ROWS.with_borrow(Clone::clone) {
        if of != build {
            continue;
        }
        times.extend(timed);
        for batch in blaze_profile::live::decode(&bytes).expect("the rows are an Arrow IPC stream") {
            let schema = batch.schema();
            let names = batch.column(schema.index_of("name").expect("a `name` column"));
            let names = names.as_dictionary::<UInt32Type>();
            let values = names.values().as_string::<i32>();
            for key in names.keys().iter() {
                *named.entry(values.value(key.expect("a name") as usize).to_owned()).or_default() += 1;
            }
        }
    }
    (named, times)
}

/// The oracle: `build`'s rows hold each of [`SPANS`] exactly once and nothing else, and its
/// client's times once.
fn each_span_once(build: u64) -> Result<(), String> {
    let (named, times) = spans_of_build(build);
    let mut wrong = Vec::new();
    for span in SPANS {
        let count = named.get(span).copied().unwrap_or(0);
        if count != 1 {
            wrong.push(format!("`{span}` {count} times"));
        }
    }
    for other in named.keys() {
        if !SPANS.contains(&other.as_str()) {
            wrong.push(format!("`{other}`, no span of a run"));
        }
    }
    match (wrong.is_empty(), times.len()) {
        (true, 1) => Ok(()),
        _ => Err(format!("build {build}: {wrong:?}; client times given {} times: {named:?}", times.len())),
    }
}

/// A warm daemon (a build of `/w`, then `src/main.rs` edited) asked a `run -q` by the thin
/// client; the build the run's rows named, once the launcher has said `Hello` and ended.
fn warm_run(seed: u64, planted: bool) -> Result<u64, String> {
    CLIENT_ROWS.with_borrow_mut(Vec::clear);
    // The program's artifact, which a `run` the thin client answers execs.
    OUTPUT.set(true);
    // A load that warns has every answer say it, which the thin client hands to the engine
    // to print: the run this test times is the one answered here.
    QUIET_LOAD.set(true);
    let ran = warm_run_of(seed, planted);
    QUIET_LOAD.set(false);
    OUTPUT.set(false);
    DROPS_A_CLIENT_SPAN.set(false);
    ran
}

fn warm_run_of(seed: u64, planted: bool) -> Result<u64, String> {
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    client.wait(&mut daemon, id);
    settle(&mut daemon, &mut client);
    daemon.driver().change(&name("w/src/main.rs"), b"fn main() { let _ = 1; }\n");

    let words: Vec<OsString> = ["run", "-q"].iter().map(OsString::from).collect();
    let Route::Cargo(asked) = route::words(&words) else { panic!("`run -q` is the thin blaze's") };
    let request = blaze_thin::request::request(
        &asked,
        blaze_thin::request::Sent {
            cwd: "/w".into(),
            root: "/w".into(),
            env: command_env(),
            files: Vec::new(),
            color: false,
            width: None,
        },
    );
    let shown = Shown::pipe(blaze_screen::Command::Run, false);
    let spoken = Spoken { asked: &asked, request: &request, shown: &shown, cwd: Path::new("/w"), pid: PID };
    let Client { conn, .. } = dial(&mut daemon);
    let mut link = SimLink { daemon: &mut daemon, conn, frames: Frames::new(), read: Default::default(), clock: Cell::new(0) };
    // `main.rs`'s three spans before the talk, on the link's clock.
    let mut timings = Timings::default();
    let at = |link: &SimLink<'_>| link.now_ns();
    let (started, main) = (at(&link), at(&link) + 1);
    timings.span(talk::EXEC_TO_MAIN, started, main);
    timings.span(talk::SOLO_CHECK, main, main + 1);
    timings.span(talk::CONNECT, main + 1, main + 2);
    let ended = talk::talk(&mut link, &spoken, |_| Ok(()), || Duration::ZERO, &mut timings);
    let Ended::Said { exec: Some(_), .. } = ended else {
        return Err(format!("seed {seed}: the run is not answered here: {ended:?}"));
    };
    let answered = timings.answered_ns.ok_or("no answer's end was timed")?;
    let exec = at(&link).max(answered) + 1;
    timings.span(blaze_api::Timed::ANSWER_TO_EXEC, answered, exec);
    let build = timings.build.ok_or("the answer named no build")?;
    let rows = timings.rows(PID, started, exec).ok_or("no rows")?;
    DROPS_A_CLIENT_SPAN.set(planted);
    link.send(talk::ROWS, &rows)?;
    for _ in 0..TURNS {
        link.daemon.turn();
        link.take()?;
        if link.read.iter().any(|it| it.id == talk::ROWS && matches!(it.body, Response::Done { .. })) {
            break;
        }
    }
    // The launcher's reader: the program said `Hello`, then its socket ended.
    steps.times.lock().hello(PID, exec + 5);
    steps.times.lock().exited(PID, exec + 9);
    // The loop's launch turn takes the program's spans to the history.
    for _ in 0..TURNS {
        if spans_of_build(build).0.contains_key(launch::HELLO_TO_EXIT) {
            break;
        }
        daemon.turn();
    }
    Ok(build)
}

/// The invariant: a warm `run` of the thin client has its build's rows hold each of its
/// seven spans (the client's five, from its exec to its program's, and the launched
/// program's two) exactly once, and its client's times once. Before 2026-11-12 a run's rows
/// came on a second connection and a program's spans nowhere.
#[test]
fn a_warm_runs_build_has_each_span_exactly_once() {
    for seed in 300..304 {
        let build = warm_run(seed, false).unwrap_or_else(|why| panic!("{why}"));
        each_span_once(build).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
    }
}

/// The planted-bug control: a daemon that drops the client's `request on the wire` as it
/// takes the rows is caught, naming the span.
#[test]
fn the_oracle_catches_a_run_whose_rows_lost_a_span() {
    let build = warm_run(300, true).unwrap_or_else(|why| panic!("{why}"));
    let caught = each_span_once(build).expect_err("a span was dropped");
    assert!(caught.contains(talk::ON_THE_WIRE), "{caught}");
}
