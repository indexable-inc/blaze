//! What the client recorded of the build it asked for, sent to the daemon's profile history as
//! the request ends ([`Request::ClientRows`]), and `BLAZE_PROFILE=<dir>`, the build's Chrome
//! trace exported from that history.
//!
//! Only the daemon writes the history; the client's own flight recorder (its start-up, its
//! connect, its printing) goes there through this request, tagged with the build id the daemon
//! gave it in [`blaze_api::Worked::build_id`]. A request that ran no build (no `Work` response)
//! sends nothing. The client's start and end times go with them, on the history's clock, and
//! become the build's `client_started_ns` and `client_ended_ns`.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use arrow_array::{
    RecordBatch,
    cast::AsArray as _,
    types::UInt32Type,
    DictionaryArray,
};
use blaze_api::{Request, Timed, client::Client};
use blaze_profile::{
    batches::{self, Process},
    clock,
    flight::{self, Cursor, Read},
    history::CLIENT_PROCESS,
    live,
    time::load,
};

use super::observe;

/// The variable that names a directory to export the build's trace to.
pub const PROFILE: &str = "BLAZE_PROFILE";

/// The build the daemon said this client's request is ([`blaze_api::Worked::build_id`]), 0
/// for none heard yet: one request a client process, so the last one heard is its.
static BUILD: AtomicU64 = AtomicU64::new(0);

/// How many steps that build computed again instead of reusing ([`blaze_api::Worked::misses`]):
/// 0 until a build is heard, and for one that recomputed nothing, whose summary is empty.
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Why that build computed again what it did, as its own `Work` carried it
/// ([`blaze_api::Worked::why`]): set once, by the one request a client process makes. Unset
/// when the daemon had not explained the build as it answered, and then the client asks
/// ([`observe::recomputed`]).
static SUMMARY: std::sync::OnceLock<(u64, Vec<blaze_api::Report>)> = std::sync::OnceLock::new();

/// The daemon said why build `build` recomputed what it did, in the build's own answer.
pub fn summarized(build: Option<u64>, why: Option<&[blaze_api::Report]>) {
    if let (Some(build), Some(why)) = (build, why) {
        // clone: the summary, a few reports, once a build; a second `Work` of the process
        // (there is none) would be dropped
        let _ = SUMMARY.set((build, why.to_vec()));
    }
}

/// The summary of build `build` its answer carried, if it carried one.
pub fn summary(build: u64) -> Option<Vec<blaze_api::Report>> {
    // clone: the summary, a few reports, once a build
    SUMMARY.get().filter(|it| it.0 == build).map(|it| it.1.clone())
}

/// The build the daemon said this client's request is, 0 for none heard.
pub fn build() -> u64 {
    BUILD.load(Ordering::Relaxed)
}

/// How many steps the build heard computed again, 0 for none heard.
pub fn misses() -> u64 {
    MISSES.load(Ordering::Relaxed)
}

/// The daemon said the request is build `build`, which computed `misses` steps again.
pub fn heard(build: Option<u64>, misses: u64) {
    if let Some(build) = build {
        BUILD.store(build, Ordering::Relaxed);
        MISSES.store(misses, Ordering::Relaxed);
    }
}

/// The thin `blaze`'s spans until it handed this command over, and the kernel's start of the
/// process, on the profile's clock (`blaze_thin::handover::Held`): set once, by `resume`.
static HANDED: std::sync::OnceLock<(Vec<Timed>, i64)> = std::sync::OnceLock::new();

/// The thin `blaze` began this command and handed it over, having timed `timed` from the
/// process's start `started_ns`: they are this command's client rows with this process's own.
pub fn handed(timed: Vec<Timed>, started_ns: i64) {
    let _ = HANDED.set((timed, started_ns));
}

/// The variable a launcher sets as it becomes another blaze (the release a workspace pins, or
/// the default; `blaze_engine`'s `program.rs`): `<pid>:<ns>`, its process and when it began on
/// the history's clock ([`launched`]). A process becomes another by `exec`, which keeps its
/// pid, so the blaze it became reads its own launcher's start and counts it in its own
/// ([`began_ns`]); a child that inherits the variable is another process and reads nothing
/// from it.
pub const LAUNCHED: &str = "BLAZE_LAUNCHED";

/// What a launcher sets [`LAUNCHED`] to: this process and when it began, the launcher's
/// before it when this one was launched itself.
pub fn launched() -> String {
    format!("{}:{}", std::process::id(), began_ns())
}

/// When the launcher that became this process began, when [`LAUNCHED`] names this process.
fn launched_ns() -> Option<i64> {
    let said = std::env::var(LAUNCHED).ok()?;
    let (pid, ns) = said.split_once(':')?;
    if pid.parse::<u32>().ok()? != std::process::id() {
        return None;
    }
    ns.parse().ok()
}

/// When this client began, on the history's clock: its first read of the clock
/// ([`blaze_profile::began`], which `main` calls first), or its launcher's when one became it,
/// so the build's `client_started_ns` counts the launcher's work (finding the pin) and the
/// second start of the program too.
fn began_ns() -> i64 {
    let own = clock::Scale::now().mono(clock::origin().0);
    launched_ns().map_or(own, |launcher| launcher.min(own))
}

/// Sends what this process recorded to the daemon as build `BUILD`'s client rows, on the
/// command's own connection `client` (one connection a command: until 2026-11-12 the rows
/// were a second one), and then, with `BLAZE_PROFILE` naming a directory, writes the build's
/// trace there as `trace.json`. Nothing when no build was heard, or in the seed. An error
/// names the step and the build.
///
/// `wait` false (a `run` about to become its program): the rows are sent and the daemon's
/// answer is not waited for, unless `BLAZE_PROFILE` is set, whose trace is read back from the
/// history the rows must be in by then. The daemon takes a request as it reads it, whether or
/// not its connection is still there when it answers.
pub fn send(client: &mut Client, wait: bool) -> Result<Option<PathBuf>, String> {
    let build = BUILD.load(Ordering::Relaxed);
    // The seed runs its engine in this process (`serve::once`): its history already records
    // this process.
    if build == 0 || !cfg!(feature = "native") {
        return Ok(None);
    }
    let request = rows(build)?;
    let dir = std::env::var_os(PROFILE).filter(|it| !it.is_empty());
    let sent = match wait || dir.is_some() {
        true => answered(client, request),
        false => client.send(request).map(|_| ()).map_err(|error| error.to_string()),
    };
    sent.map_err(|why| format!("sending build {build}'s client rows to the daemon: {why}"))?;
    let Some(dir) = dir else { return Ok(None) };
    export(build, Path::new(&dir)).map(Some)
}

/// `request` sent on `client` and its answer waited for: an error the daemon said is the
/// error.
fn answered(client: &mut Client, request: Request) -> Result<(), String> {
    let mut errors = Vec::new();
    let ok = client
        .call(request, |response| {
            if let blaze_api::Response::Error(error) = response {
                errors.push(error.what.to_string()); // alloc: error path, said once it is done
            }
        })
        .map_err(|error| error.to_string())?;
    match (ok, errors.is_empty()) {
        (true, true) => Ok(()),
        (_, false) => Err(errors.join("\n")),
        (false, true) => Err("the daemon answered not ok and said no error".to_owned()),
    }
}

/// This process's rows since it began, every one given `build`, as the request that sends
/// them: with the thin `blaze`'s spans when it handed the command over ([`handed`]), whose
/// start is then the command's.
fn rows(build: u64) -> Result<Request, String> {
    let ended_ns = clock::mono_ns();
    let (thin, thin_ns) = match HANDED.get() {
        Some((timed, started_ns)) => (timed.as_slice(), Some(*started_ns)),
        None => (&[][..], None),
    };
    let started_ns = thin_ns.map_or(began_ns(), |thin| thin.min(began_ns()));
    let tagged = u32::try_from(build)
        .map_err(|_| format!("build {build} is past the recorder's 32-bit build ids"))?;
    let mut read = Read::default();
    flight::read(&mut Cursor::default(), &mut read);
    read.spans.iter_mut().for_each(|it| it.build = tagged);
    read.events.iter_mut().for_each(|it| it.build = tagged);
    read.folds.iter_mut().for_each(|it| it.build = tagged);
    let pid = std::process::id();
    let process = Process { pid, name: CLIENT_PROCESS, commit: None };
    let spans = spans_batch(build, pid, std::mem::take(&mut read.spans), thin);
    Ok(Request::ClientRows {
        pid,
        build,
        // This client's spans, its recorder's and the thin `blaze`'s, are sent as `spans`.
        timed: Vec::new(),
        events: encoded(build, "events", batches::events(&read.events, process))?,
        folded: encoded(build, "folded", batches::folded(&read.folds, process))?,
        spans: encoded(build, "spans", spans)?,
        ended_ns,
        started_ns,
    })
}

/// `batch`, build `build`'s client rows of `table`, as an Arrow IPC stream: none for no rows.
fn encoded<E: std::fmt::Display>(
    build: u64,
    table: &str,
    batch: Result<arrow_array::RecordBatch, E>,
) -> Result<Vec<u8>, String> {
    let batch =
        batch.map_err(|error| format!("making build {build}'s client {table} rows: {error}"))?;
    // alloc: the request's bytes, once a client
    let mut bytes = Vec::new();
    if batch.num_rows() > 0 {
        live::encode(&batch, &mut bytes)
            .map_err(|error| format!("encoding build {build}'s client {table} rows: {error}"))?;
    }
    Ok(bytes)
}

/// `BLAZE_PROFILE`: build `build`'s Chrome trace from the history, as `blaze debug time
/// --trace` makes it, written to `<dir>/trace.json`; its path.
fn export(build: u64, dir: &Path) -> Result<PathBuf, String> {
    let (duckdb, history) = observe::history_paths().map_err(|why| format!("{PROFILE}: {why}"))?;
    let source = load::Source { duckdb: &duckdb, history: &history };
    let trace = load::trace_of(&source, build)
        .map_err(|error| format!("{PROFILE}: build {build}'s trace: {error}"))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("{PROFILE}: making {}: {error}", dir.display()))?;
    let path = dir.join("trace.json");
    std::fs::write(&path, trace)
        .map_err(|error| format!("{PROFILE}: writing {}: {error}", path.display()))?;
    Ok(path)
}

/// The `spans` rows of client `pid`'s request for build `build` as the daemon's history takes
/// them ([`blaze_profile::history::ClientRows::spans`]): `spans` as the client sent them, or,
/// for a client that times its spans by name (`timed`, the thin `blaze`, which has no flight
/// recorder), those made rows of the client's process here, one root span each. A client
/// that sent both is refused, naming the build: its spans would be counted twice (the engine
/// sends a handed-over command's spans of the thin `blaze` inside its own, [`spans_batch`]).
pub fn spans_of(build: u64, pid: u32, spans: Vec<u8>, timed: &[Timed]) -> Result<Vec<u8>, String> {
    if timed.is_empty() {
        return Ok(spans);
    }
    if !spans.is_empty() {
        return Err(format!(
            "client {pid} sent build {build}'s spans both as rows and by name: a client sends them one way"
        ));
    }
    encoded(build, "spans", spans_batch(build, pid, Vec::new(), timed))
}

/// Client `pid`'s `spans` rows of build `build`: `recorded` (a flight recorder's, named by
/// their sites) and then `timed` (spans by name), one root row each, under span ids after the
/// recorded ones, in one batch of the history's table schema.
fn spans_batch(build: u64, pid: u32, mut recorded: Vec<flight::SpanRow>, timed: &[Timed]) -> Result<RecordBatch, String> {
    let tagged = u32::try_from(build)
        .map_err(|_| format!("build {build} is past the recorder's 32-bit build ids"))?;
    let before = recorded.len();
    let first = recorded.iter().map(|it| it.span_id).max().unwrap_or(0) + 1;
    // alloc: a client's few spans more, once a request
    recorded.extend(timed.iter().enumerate().map(|(at, it)| flight::SpanRow {
        build: tagged,
        thread: 0,
        thread_name: 0,
        span_id: first + at as u64,
        parent: 0,
        depth: 0,
        site: 0,
        start_ns: it.start_ns,
        dur_ns: it.end_ns - it.start_ns,
        alloc: None,
        copied: None,
        krate: 0,
        function: 0,
        unit: 0,
        self_cpu_ns: None,
        typed: [0; blaze_profile::span::TYPED],
        folded_spans: 0,
        folded_ns: 0,
    }));
    let process = Process { pid, name: CLIENT_PROCESS, commit: None };
    let batch = batches::spans(&recorded, process)
        .map_err(|error| format!("making build {build}'s client spans of client {pid}: {error}"))?;
    if timed.is_empty() {
        return Ok(batch);
    }
    // The timed rows' names are the client's, not a site of this process's: the name column
    // made again, the recorded rows' names as made and the timed rows' after them, and their
    // target blaze's steps' (`blaze`).
    let schema = batch.schema();
    let column = |named: &str| {
        schema.index_of(named).map_err(|error| format!("the spans table has no `{named}` column: {error}"))
    };
    let (name, target) = (column("name")?, column("target")?);
    let names = renamed(&batch, name, before, timed.iter().map(|it| it.name.as_str()))?;
    let targets = renamed(&batch, target, before, timed.iter().map(|_| "blaze"))?;
    let mut columns = batch.columns().to_vec(); // alloc: a batch's column list, once a request
    columns[name] = std::sync::Arc::new(names);
    columns[target] = std::sync::Arc::new(targets);
    RecordBatch::try_new(schema, columns)
        .map_err(|error| format!("naming build {build}'s client spans of client {pid}: {error}"))
}

/// Column `at` of `batch` (a dictionary of strings) made again: its first `before` values as
/// they are, then `after`.
fn renamed<'a>(
    batch: &'a RecordBatch,
    at: usize,
    before: usize,
    after: impl Iterator<Item = &'a str>,
) -> Result<DictionaryArray<UInt32Type>, String> {
    let field = batch.schema_ref().field(at).name();
    let column = batch
        .column(at)
        .as_dictionary_opt::<UInt32Type>()
        .ok_or_else(|| format!("the spans table's `{field}` is not a dictionary of u32 keys"))?;
    let values = column
        .values()
        .as_string_opt::<i32>()
        .ok_or_else(|| format!("the spans table's `{field}` is not a dictionary of strings"))?;
    let made = column.keys().iter().take(before).map(|key| key.map(|key| values.value(key as usize)));
    Ok(made.chain(after.map(Some)).collect())
}

#[cfg(test)]
mod tests {
    use arrow_array::{Array as _, Int64Array, cast::AsArray as _, types::UInt32Type};

    use super::*;

    /// The invariant: a client's spans sent by name become one `spans` row each, of its build
    /// and process, under its own name and with its own start and length, in the history's
    /// table schema. Controls: a client that sent its spans both ways is refused naming the
    /// build, and one that sent none by name keeps its rows as sent.
    #[test]
    fn timed_spans_become_the_clients_rows_once_each() {
        let timed = [
            blaze_api::Timed { name: "client: connect".to_owned(), start_ns: 10, end_ns: 15 },
            blaze_api::Timed { name: blaze_api::Timed::ANSWER_TO_EXEC.to_owned(), start_ns: 20, end_ns: 32 },
        ];
        let bytes = spans_of(7, 41, Vec::new(), &timed).expect("made rows");
        let batches = blaze_profile::live::decode(&bytes).expect("an Arrow IPC stream");
        let [batch] = batches.as_slice() else { panic!("one batch: {}", batches.len()) };
        let schema = batch.schema();
        // clone: a column's `Arc`, a few in a test
        let column = |name: &str| batch.column(schema.index_of(name).expect(name)).clone();
        let names = column("name");
        let names = names.as_dictionary::<UInt32Type>();
        let values = names.values().as_string::<i32>();
        let named: Vec<&str> = names.keys().iter().map(|it| values.value(it.expect("a name") as usize)).collect();
        assert_eq!(named, ["client: connect", blaze_api::Timed::ANSWER_TO_EXEC]);
        let starts = column("start_ns");
        let starts = starts.as_any().downcast_ref::<Int64Array>().expect("i64");
        let lengths = column("dur_ns");
        let lengths = lengths.as_any().downcast_ref::<Int64Array>().expect("i64");
        assert_eq!((starts.value(0), lengths.value(0), starts.value(1), lengths.value(1)), (10, 5, 20, 12));
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(column("process").len(), 2);

        let both = spans_of(7, 41, vec![1], &timed).expect_err("both ways");
        assert!(both.contains("build 7"), "{both}");
        assert_eq!(spans_of(7, 41, vec![1, 2], &[]), Ok(vec![1, 2]));
    }
}

#[cfg(test)]
mod handed_tests {
    use std::{cell::RefCell, rc::Rc};

    use arrow_array::{Array as _, UInt64Array, cast::AsArray as _};

    use super::*;

    fn names(batch: &RecordBatch, column: &str) -> Vec<String> {
        let at = batch.schema().index_of(column).expect(column);
        let dict = batch.column(at).as_dictionary::<UInt32Type>();
        let values = dict.values().as_string::<i32>();
        dict.keys().iter().map(|it| values.value(it.expect("a value") as usize).to_owned()).collect()
    }

    fn recorded(span_id: u64, start_ns: i64) -> flight::SpanRow {
        flight::SpanRow {
            build: 7,
            thread: 1,
            thread_name: 0,
            span_id,
            parent: 0,
            depth: 0,
            site: 0,
            start_ns,
            dur_ns: 3,
            alloc: None,
            copied: None,
            krate: 0,
            function: 0,
            unit: 0,
            self_cpu_ns: None,
            typed: [0; blaze_profile::span::TYPED],
            folded_spans: 0,
            folded_ns: 0,
        }
    }

    /// The invariant: a command the thin `blaze` handed over has, in its build's one `spans`
    /// batch, every row of the engine's recorder as the recorder alone would make it, then each
    /// of the thin `blaze`'s spans once under its own name, all under distinct span ids.
    /// Control (the planted bug: the handed spans dropped): with none handed, none of the thin
    /// `blaze`'s names is there, so the oracle sees the loss.
    #[test]
    fn a_handed_over_commands_thin_spans_are_its_rows_once_beside_the_engines() {
        let engine = vec![recorded(1, 100), recorded(5, 110)];
        let thin = [
            Timed { name: "client: exec to main".to_owned(), start_ns: 0, end_ns: 4 },
            Timed { name: "client: solo check".to_owned(), start_ns: 4, end_ns: 9 },
            Timed { name: "client: connect".to_owned(), start_ns: 9, end_ns: 12 },
        ];
        let alone = spans_batch(7, 41, engine.clone(), &[]).expect("the engine's rows"); // clone: a test's rows
        let both = spans_batch(7, 41, engine.clone(), &thin).expect("both"); // clone: a test's rows
        assert_eq!(both.num_rows(), engine.len() + thin.len());
        let named = names(&both, "name");
        assert_eq!(named[..engine.len()], names(&alone, "name")[..]);
        for span in &thin {
            assert_eq!(named.iter().filter(|it| **it == span.name).count(), 1, "{}: {named:?}", span.name);
        }
        assert_eq!(names(&both, "target")[engine.len()..], ["blaze", "blaze", "blaze"]);
        let ids = both.column(both.schema().index_of("span_id").expect("span_id"));
        let ids = ids.as_any().downcast_ref::<UInt64Array>().expect("u64 span ids");
        let mut seen: Vec<u64> = ids.iter().map(|it| it.expect("an id")).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), both.num_rows(), "distinct span ids");
        // Encoded, it is one stream the history reads whole.
        let bytes = encoded(7, "spans", Ok::<_, std::convert::Infallible>(both)).expect("encoded");
        let read = blaze_profile::live::decode(&bytes).expect("an Arrow IPC stream");
        assert_eq!(read.iter().map(RecordBatch::num_rows).sum::<usize>(), engine.len() + thin.len());

        // The control.
        let named = names(&alone, "name");
        assert!(thin.iter().all(|span| !named.contains(&span.name)), "{named:?}");
    }

    /// A daemon in a test: every frame it is handed is kept, and each request is answered with
    /// `answer` and its `Done`.
    struct Kept {
        sent: Rc<RefCell<usize>>,
        replies: Vec<u8>,
        error: bool,
    }

    impl blaze_api::client::Local for Kept {
        fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
            let mut frames = blaze_api::frames::Frames::new();
            frames.push(frame);
            let item = frames.next_ref().expect("a frame").expect("whole");
            let id = blaze_api::frames::decode::<Request>(item).expect("a request").id;
            *self.sent.borrow_mut() += 1;
            if self.error {
                let error = blaze_api::ApiError::new("refused in the test", None);
                let said = blaze_api::Envelope::new(id, blaze_api::Response::Error(error));
                self.replies.extend(blaze_api::frames::encode(&said).expect("encoded"));
            }
            let done = blaze_api::Envelope::new(id, blaze_api::Response::Done { ok: !self.error });
            self.replies.extend(blaze_api::frames::encode(&done).expect("encoded"));
            Ok(())
        }

        fn receive(&mut self, into: &mut Vec<u8>) -> std::io::Result<()> {
            into.append(&mut self.replies);
            Ok(())
        }

        fn close(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rows_request() -> Request {
        Request::ClientRows {
            pid: 41,
            build: 7,
            spans: Vec::new(),
            timed: Vec::new(),
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns: 2,
            started_ns: 1,
        }
    }

    /// The invariant: the rows go on the connection they are given, once, and the daemon's
    /// answer is waited for on it. Control: a daemon that refuses them is the error, so a
    /// refusal is not taken for a send.
    #[test]
    fn the_rows_go_on_the_commands_own_connection_once() {
        let sent = Rc::default();
        let link = Kept { sent: Rc::clone(&sent), replies: Vec::new(), error: false };
        let mut client = Client::local(Box::new(link));
        answered(&mut client, rows_request()).expect("answered");
        assert_eq!(*sent.borrow(), 1);

        let sent = Rc::default();
        let link = Kept { sent: Rc::clone(&sent), replies: Vec::new(), error: true };
        let mut client = Client::local(Box::new(link));
        let refused = answered(&mut client, rows_request()).expect_err("refused");
        assert!(refused.contains("refused in the test"), "{refused}");
        assert_eq!(*sent.borrow(), 1);
    }
}
