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

use blaze_api::Request;
use blaze_profile::{
    batches::{self, Process},
    clock,
    flight::{self, Cursor, Read},
    history::CLIENT_PROCESS,
    live,
    time::load,
};

use super::{daemon::Start, observe};

/// The variable that names a directory to export the build's trace to.
pub const PROFILE: &str = "BLAZE_PROFILE";

/// The build the daemon said this client's request is ([`blaze_api::Worked::build_id`]), 0
/// for none heard yet: one request a client process, so the last one heard is its.
static BUILD: AtomicU64 = AtomicU64::new(0);

/// How many steps that build computed again instead of reusing ([`blaze_api::Worked::misses`]):
/// 0 until a build is heard, and for one that recomputed nothing, whose summary is empty.
static MISSES: AtomicU64 = AtomicU64::new(0);

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

/// Sends what this process recorded to the daemon as build `BUILD`'s client rows, and then,
/// with `BLAZE_PROFILE` naming a directory, writes the build's trace there as `trace.json`.
/// Nothing when no build was heard, or in the seed. An error names the step and the build.
pub fn send() -> Result<Option<PathBuf>, String> {
    let build = BUILD.load(Ordering::Relaxed);
    // The seed runs its engine in this process (`serve::once`): its history already records
    // this process, and a second connection would start a second engine.
    if build == 0 || !cfg!(feature = "native") {
        return Ok(None);
    }
    let request = rows(build)?;
    observe::ask(request, Start::Never, |_| Ok(()))
        .map_err(|why| format!("sending build {build}'s client rows to the daemon: {why}"))?;
    let Some(dir) = std::env::var_os(PROFILE).filter(|it| !it.is_empty()) else { return Ok(None) };
    export(build, Path::new(&dir)).map(Some)
}

/// This process's rows since it began, every one given `build`, as the request that sends
/// them.
fn rows(build: u64) -> Result<Request, String> {
    let ended_ns = clock::mono_ns();
    let started_ns = began_ns();
    let tagged = u32::try_from(build)
        .map_err(|_| format!("build {build} is past the recorder's 32-bit build ids"))?;
    let mut read = Read::default();
    flight::read(&mut Cursor::default(), &mut read);
    read.spans.iter_mut().for_each(|it| it.build = tagged);
    read.events.iter_mut().for_each(|it| it.build = tagged);
    read.folds.iter_mut().for_each(|it| it.build = tagged);
    let process = Process { pid: std::process::id(), name: CLIENT_PROCESS, commit: None };
    Ok(Request::ClientRows {
        build,
        events: encoded(build, "events", batches::events(&read.events, process))?,
        folded: encoded(build, "folded", batches::folded(&read.folds, process))?,
        spans: encoded(build, "spans", batches::spans(&read.spans, process))?,
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
