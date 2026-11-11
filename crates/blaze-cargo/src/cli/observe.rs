//! blaze's looks at itself (docs/profiling.md): `blaze explore`, the explorer's link to the
//! daemon (docs/v2/plan/design/explore.md); `blaze debug profile`, `blaze debug live`
//! and `blaze debug sql`, the daemon's flight recorder and its profile history; `blaze
//! explain`, a rule's long text, the last build's failure of a package with all it blocks, or a
//! named view of that history; and `blaze why`, what the last build computed again and why.
//!
//! `blaze debug sql` and `blaze explain` run the DuckDB `BLAZE_DUCKDB` names
//! ([`crate::serve::observe::DUCKDB`]), here in
//! the CLI over the history's files, after asking the daemon that writes the history to flush
//! ([`history_source`]), so they read every row it recorded before they asked, each once.
//! Unset, they run the DuckDB blaze pins, fetched by its SHA-256 on first use (`duckdb.rs`):
//! nothing looks for a DuckDB on `PATH`.

use std::{
    ffi::OsString,
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use arrow_array::{Array, RecordBatch, cast::AsArray, types};
use blaze_api::{Request, ResponseRef};
use blaze_core::exit::Exit;
use blaze_profile::{
    history, live, schema, slow, sql,
    time::{load, report::By},
    views,
};
use serde_json::{Map, Value};

use super::{
    daemon::{self, Api, Start},
    say,
};

const EXPLORE_USAGE: &str = "blaze explore [--dev] [--print]: the explorer's link, opened in the browser (`--print`: printed only; \
                             `--dev`: the page's development server, http://localhost:5173, may connect too)";
const DEBUG_USAGE: &str = "blaze debug profile [--seconds N] | blaze debug live | blaze debug sql [--json] QUERY \
                           | blaze debug hot [--top N] [--last-build] | blaze debug time [--build N] [--by unit|item|wait] [--top N] \
                           [--match TEXT] [--cores [LINES]] [--html FILE] [--trace FILE] \
                           | blaze debug gate [--build N] [--history DIR] [--json] [--summary FILE] \
                           | blaze debug locks [--build N] [--top N] [--json]";
const EXPLAIN_USAGE: &str =
    "blaze explain RULE | PACKAGE [--json] | memory [BUILD] [limit=N] [--json] | VIEW [NAME=VALUE ...] [--json]";
const MEMORY_USAGE: &str = "blaze explain memory [BUILD] [limit=N] [--json]";

/// The parts of `blaze explain memory`, each a view of `sql/examples.sql`, with its heading.
const MEMORY_PARTS: [(&str, &str, &str); 4] = [
    ("timeline", "memory_timeline", "the memory, minute by minute"),
    ("peak_tasks", "memory_peak_tasks", "the tasks that held the most at once"),
    ("at_peak", "memory_at_peak", "the memory by area at the footprint's peak"),
    ("sites", "memory_sites", "what churns the global heap, by call site"),
];
const WHY_USAGE: &str = "blaze why [ITEM] [--all] [--unexplained] [--json]";
const SLOW_USAGE: &str = "blaze slow [--build N] [--top N] [--json]";

/// `blaze explore`: the daemon's explorer started if it is not, its link printed (it carries
/// the session's token: on this terminal only) and opened.
pub fn explore(words: Vec<OsString>, color: bool) -> i32 {
    let (mut dev, mut print) = (false, false);
    for word in words {
        match word.to_str() {
            Some("--dev") => dev = true,
            Some("--print") => print = true,
            _ => {
                return refused(
                    &format!("{EXPLORE_USAGE}, not `{}`", word.to_string_lossy()),
                    Exit::Usage.code(),
                    color,
                );
            }
        }
    }
    let mut link = None;
    let asked = ask(Request::Explore { dev }, Start::IfMissing, |response| {
        if let ResponseRef::Explored { link: it } = response {
            link = Some(it.to_owned());
        }
        Ok(())
    });
    if let Err(what) = asked {
        return refused(&what, Exit::Failed.code(), color);
    }
    let Some(link) = link else {
        return refused(
            "the daemon answered `blaze explore` with no link",
            Exit::Failed.code(),
            color,
        );
    };
    println!("{link}");
    if !print && let Err(why) = open(&link) {
        eprint!(
            "{}",
            say::warning(&format!("the link was not opened ({why}): open it in a browser"), color)
        );
    }
    0
}

/// The browser asked to open `link`: `open` on macOS, `xdg-open` elsewhere.
fn open(link: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let status = std::process::Command::new(program)
        .arg(link)
        .status()
        .map_err(|error| format!("running `{program}`: {error}"))?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("`{program}` exited {status}")),
    }
}

/// `blaze debug profile`, `live`, `sql`, `hot`, `time`, `gate` or `locks`.
pub fn debug(words: Vec<OsString>, color: bool) -> i32 {
    let mut words = words.into_iter();
    let verb = words.next();
    let result = match verb.as_ref().and_then(|it| it.to_str()) {
        Some("profile") => profile(words.collect()),
        Some("live") => match words.next() {
            None => watch(),
            Some(word) => Err(Refusal::Usage(format!(
                "blaze debug live takes nothing, not `{}`",
                word.to_string_lossy()
            ))),
        },
        Some("sql") => query(words.collect()),
        Some("hot") => hot(words.collect()),
        Some("time") => time(words.collect()),
        Some("gate") => gate(words.collect()),
        Some("locks") => locks(words.collect()),
        _ => Err(Refusal::Usage(DEBUG_USAGE.to_owned())),
    };
    said(result, color)
}

/// `blaze debug hot [--top N] [--last-build]`: the N keys (20) that took most time and each
/// kind of wait's quantiles, from the daemon's live sketches, since it began or over its last
/// build.
fn hot(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut top, mut since) = (20u32, blaze_api::HotSince::Start);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--last-build" => since = blaze_api::HotSince::LastBuild,
            "--top" => {
                top = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .ok_or_else(|| Refusal::Usage("`--top` takes a number of keys".to_owned()))?;
            }
            _ => {
                return Err(Refusal::Usage(format!(
                    "blaze debug hot [--top N] [--last-build], not `{word}`"
                )));
            }
        }
    }
    let mut text = String::new();
    ask(Request::Hot { top, since }, Start::Never, |response| {
        if let ResponseRef::Hot(hot) = response {
            blaze_ui::api::hot_lines(&mut text, &hot);
        }
        Ok(())
    })?;
    if text.is_empty() {
        return Err(Refusal::Failed(
            "the daemon answered `blaze debug hot` with no answer".to_owned(),
        ));
    }
    print!("{text}");
    Ok(())
}

/// `blaze debug time`: where a build's time went, per unit, item or wait, as running, parked,
/// queued and uncovered time, with its critical path (blaze_profile::time).
fn time(words: Vec<OsString>) -> Result<(), Refusal> {
    let usage = |what: &str| {
        Refusal::Usage(format!(
            "blaze debug time [--build N] [--by unit|item|wait] [--top N] [--match TEXT] [--cores [LINES]] [--html FILE] [--trace FILE]: {what}"
        ))
    };
    let (mut build, mut by, mut top, mut cores) = (None, By::Unit, 20usize, None);
    let (mut matching, mut html, mut trace): (Option<String>, Option<PathBuf>, Option<PathBuf>) =
        (None, None, None);
    let mut words = texts(words)?.into_iter().peekable();
    while let Some(word) = words.next() {
        let mut value =
            |what: &str| words.next().ok_or_else(|| usage(&format!("`{word}` takes {what}")));
        match word.as_str() {
            "--build" => {
                build = Some(
                    value("a build id")?
                        .parse()
                        .map_err(|_| usage("`--build` takes a build id"))?,
                )
            }
            "--by" => {
                let name = value(By::NAMES)?;
                by = By::of(&name)
                    .ok_or_else(|| usage(&format!("`--by` takes {}, not `{name}`", By::NAMES)))?;
            }
            "--top" => {
                top = value("a number of rows")?
                    .parse()
                    .map_err(|_| usage("`--top` takes a number of rows"))?
            }
            "--match" => matching = Some(value("a text the rows' keys hold")?),
            "--html" => html = Some(PathBuf::from(value("a file")?)),
            "--trace" => trace = Some(PathBuf::from(value("a file")?)),
            "--cores" => {
                let lines =
                    words.next_if(|it| it.parse::<usize>().is_ok()).map(|it| it.parse::<usize>());
                cores = Some(
                    lines
                        .transpose()
                        .map_err(|_| usage("`--cores` takes a number of lines"))?
                        .unwrap_or(40),
                );
            }
            _ => return Err(usage(&format!("not `{word}`"))),
        }
    }
    let (duckdb, dir) = history_source()?;
    let source = load::Source { duckdb: &duckdb, history: &dir };
    let ask = load::Ask {
        build,
        by,
        top,
        matching: matching.as_deref(),
        cores,
        html: html.as_deref(),
        trace: trace.as_deref(),
    };
    let text = load::run(&source, &ask).map_err(|error| error.to_string())?;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

/// `blaze explain RULE`: the long text of a rule a report names (`sandbox:link-dir`,
/// [`blaze_report::rules`]). `blaze explain PACKAGE [--json]`: the last build's failure of the
/// package, with every package it blocks ([`last_failure`]). `blaze explain VIEW [NAME=VALUE
/// ...] [--json]`: the view's SQL with its arguments bound, run over the history.
pub fn explain(words: Vec<OsString>, color: bool) -> i32 {
    said(explained(words), color)
}

/// `blaze why [ITEM] [--all] [--unexplained] [--json]`: what the daemon's last build computed
/// again instead of reusing, grouped by why on the daemon (each group's first items, every one
/// with `--all`); with an item (a function, a unit, a build script, by path), its chain of
/// reasons; with `--unexplained`, each step blaze has no recorded reason for, which is a bug
/// in blaze (`Request::Rebuilt`).
pub fn why(words: Vec<OsString>, color: bool) -> i32 {
    said(rebuilt(words), color)
}

fn rebuilt(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut item, mut json, mut all, mut unexplained) = (None, false, false, false);
    for word in texts(words)? {
        match word.as_str() {
            "--json" => json = true,
            "--all" => all = true,
            "--unexplained" => unexplained = true,
            _ if word.starts_with('-') => {
                return Err(Refusal::Usage(format!("{WHY_USAGE}, not `{word}`")));
            }
            _ if item.is_none() => item = Some(word),
            _ => return Err(Refusal::Usage(format!("{WHY_USAGE}: one item, not `{word}` too"))),
        }
    }
    let mut answer = None;
    // The workspace holding this directory is the one whose last build is asked of.
    let cwd = std::env::current_dir()
        .map_err(|error| format!("`blaze why` reading the current directory: {error}"))?;
    let Some(cwd) = cwd.to_str().map(str::to_owned) else {
        return Err(Refusal::Failed(format!(
            "`blaze why`: the current directory {} is not UTF-8",
            cwd.display()
        )));
    };
    let cwd = Some(cwd);
    ask(Request::Rebuilt { all, cwd, item, build: None, unexplained }, Start::Never, |response| {
        if let ResponseRef::Rebuilt(it) = response {
            answer = Some(it);
        }
        Ok(())
    })?;
    let Some(rebuilt) = answer else {
        return Err(Refusal::Failed("the daemon answered `blaze why` with no answer".to_owned()));
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&rebuilt)
                .map_err(|error| format!("`blaze why`'s answer as JSON: {error}"))?
        );
        return Ok(());
    }
    let mut text = String::new();
    match unexplained {
        true => blaze_ui::why::unexplained(&mut text, &rebuilt),
        false => blaze_ui::why::lines(&mut text, &rebuilt),
    }
    print!("{text}");
    Ok(())
}

/// Why the build this process asked for recomputed what it did, as the daemon sums it up once
/// the build has ended (`Rebuilt::summary`, docs/v2/plan/design/why.md): a report a root
/// cause. None when this process heard of no build, in the seed (whose engine is this
/// process: a second connection would start a second), when no daemon is running, and when
/// the reasons the daemon keeps are by now another build's (`blaze why` and the history's
/// `misses` table still have them). None, and nothing asked, when the build said it computed
/// nothing again (`Worked::misses` of 0): its summary is of those misses, so it is empty, and
/// a no-op build makes no second round trip and waits for no history thread. The daemon's
/// error otherwise.
pub(super) fn recomputed() -> Result<Vec<blaze_api::Report>, String> {
    let build = super::client_rows::build();
    if build == 0 || super::client_rows::misses() == 0 || !cfg!(feature = "native") {
        return Ok(Vec::new());
    }
    let _span =
        tracing::info_span!(target: "blaze", "client: ask why the build recomputed", build)
            .entered();
    let mut summary = Vec::new();
    let request =
        Request::Rebuilt { all: false, cwd: None, item: None, build: Some(build), unexplained: false };
    ask_if_running(request, Start::Never, |response| {
        if let ResponseRef::Rebuilt(it) = response
            && it.build == Some(build)
        {
            summary = it.summary;
        }
        Ok(())
    })
    .map_err(|why| format!("why build {build} recomputed what it did is not known: {why}"))?;
    Ok(summary)
}

/// `blaze slow [--build N] [--top N] [--json]`: what made a build slow, ranked by the wall time
/// each fix would save, and what its profile could not see (blaze_profile::slow), of the last
/// build that ended unless `--build` says.
pub fn slow(words: Vec<OsString>, color: bool) -> i32 {
    said(slowed(words), color)
}

fn slowed(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut build, mut top, mut json) = (None, 10usize, false);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        let mut number = |what: &str| -> Result<u64, Refusal> {
            words
                .next()
                .and_then(|it| it.parse().ok())
                .ok_or_else(|| Refusal::Usage(format!("{SLOW_USAGE}: `{word}` takes {what}")))
        };
        match word.as_str() {
            "--build" => build = Some(number("a build id")?),
            "--top" => top = usize::try_from(number("a number of findings")?).unwrap_or(usize::MAX),
            "--json" => json = true,
            _ => return Err(Refusal::Usage(format!("{SLOW_USAGE}, not `{word}`"))),
        }
    }
    let (duckdb, dir) = history_source()?;
    let source = load::Source { duckdb: &duckdb, history: &dir };
    let report =
        slow::load::run(&source, build).map_err(|error| format!("`blaze slow`: {error}"))?;
    let text = match json {
        true => {
            serde_json::to_string(&report)
                .map_err(|error| format!("`blaze slow`'s report as JSON: {error}"))?
                + "\n"
        }
        false => slow::text(&report, top),
    };
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

/// `blaze debug gate [--build N] [--history DIR] [--json] [--summary FILE]`: the profile's gate
/// (blaze_profile::gate) over a build of the history (the daemon's, or `--history`'s), each
/// check's outcome printed, as JSON with `--json`; `--summary` writes the build's per-build
/// summary (`build_summary`) as Parquet, after the verdict is printed. It exits non-zero unless
/// every check passed (one that could not run fails it too) and the summary was written.
fn gate(words: Vec<OsString>) -> Result<(), Refusal> {
    const USAGE: &str = "blaze debug gate [--build N] [--history DIR] [--json] [--summary FILE]";
    let (mut build, mut dir, mut json, mut summary) = (None, None, false, None);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        let mut next = |what: &str| {
            words.next().ok_or_else(|| Refusal::Usage(format!("{USAGE}: `{word}` takes {what}")))
        };
        match word.as_str() {
            "--build" => {
                build =
                    Some(next("a build id")?.parse::<u64>().map_err(|_| {
                        Refusal::Usage(format!("{USAGE}: `--build` takes a build id"))
                    })?);
            }
            "--history" => dir = Some(PathBuf::from(next("a history's directory")?)),
            "--summary" => summary = Some(PathBuf::from(next("a Parquet file to write")?)),
            "--json" => json = true,
            _ => return Err(Refusal::Usage(format!("{USAGE}, not `{word}`"))),
        }
    }
    let (duckdb, dir) = match dir {
        Some(dir) => {
            (super::duckdb::path()?, dir)
        }
        None => history_source()?,
    };
    let verdict =
        blaze_profile::gate::check(&duckdb, &dir, build).map_err(|error| error.to_string())?;
    let text = match json {
        true => {
            serde_json::to_string(&verdict)
                .map_err(|error| format!("the gate's verdict as JSON: {error}"))?
                + "\n"
        }
        false => verdict.to_string(),
    };
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))?;
    // After the verdict is printed, so a summary that fails to write never hides it.
    if let Some(out) = summary {
        blaze_profile::gate::summary(&duckdb, &dir, verdict.build, &out)
            .map_err(|error| error.to_string())?;
    }
    match verdict.passed() {
        true => Ok(()),
        false => Err(Refusal::Failed(format!("the profile's gate: {}", verdict.summary()))),
    }
}

/// `blaze debug locks [--build N] [--top N] [--json]`: the named locks a build (the last that
/// ended unless `--build` says) waited for, the most waited first (20 unless `--top` says), each
/// with its call sites, the spans its waiters and holders were in, its share of the pool
/// workers' time and what to use instead; then the build's stalls (blaze_profile::locks).
fn locks(words: Vec<OsString>) -> Result<(), Refusal> {
    const USAGE: &str = "blaze debug locks [--build N] [--top N] [--json]";
    let (mut build, mut top, mut json) = (None, 20usize, false);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        let mut number = |what: &str| -> Result<u64, Refusal> {
            words
                .next()
                .and_then(|it| it.parse().ok())
                .ok_or_else(|| Refusal::Usage(format!("{USAGE}: `{word}` takes {what}")))
        };
        match word.as_str() {
            "--build" => build = Some(number("a build id")?),
            "--top" => top = usize::try_from(number("a number of locks")?).unwrap_or(usize::MAX),
            "--json" => json = true,
            _ => return Err(Refusal::Usage(format!("{USAGE}, not `{word}`"))),
        }
    }
    let (duckdb, dir) = history_source()?;
    let report = blaze_profile::locks::load(&duckdb, &dir, build, top)
        .map_err(|error| error.to_string())?;
    let text = match json {
        true => {
            serde_json::to_string(&report)
                .map_err(|error| format!("`blaze debug locks`'s report as JSON: {error}"))?
                + "\n"
        }
        false => blaze_profile::locks::text(&report),
    };
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

enum Refusal {
    Usage(String),
    Failed(String),
}

fn said(result: Result<(), Refusal>, color: bool) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Refusal::Usage(what)) => refused(&what, Exit::Usage.code(), color),
        Err(Refusal::Failed(what)) => refused(&what, Exit::Failed.code(), color),
    }
}

impl From<String> for Refusal {
    fn from(what: String) -> Refusal {
        Refusal::Failed(what)
    }
}

fn refused(what: &str, code: i32, color: bool) -> i32 {
    eprint!("{}", say::error(what, color));
    code
}

/// The words as text: a request carries no other.
fn texts(words: Vec<OsString>) -> Result<Vec<String>, Refusal> {
    words
        .into_iter()
        .map(|it| {
            it.into_string()
                .map_err(|it| Refusal::Usage(format!("{} is not UTF-8", it.to_string_lossy())))
        })
        .collect()
}

/// `request` asked of the daemon, each response but its errors handed to `on`; the daemon's
/// errors, and a Done not ok, are the error. With [`Start::Never`], no daemon running is one.
pub(super) fn ask(
    request: Request,
    start: Start,
    on: impl FnMut(ResponseRef<'_>) -> Result<(), String>,
) -> Result<(), String> {
    match ask_if_running(request, start, on)? {
        true => Ok(()),
        false => Err("no daemon is running: a build, or `blaze serve`, starts one".to_owned()),
    }
}

/// [`ask`], where no daemon running (under [`Start::Never`]) is not an error: whether one
/// was asked.
fn ask_if_running(
    request: Request,
    start: Start,
    mut on: impl FnMut(ResponseRef<'_>) -> Result<(), String>,
) -> Result<bool, String> {
    let mut client = match daemon::connect(start) {
        Ok(Some(client)) => client,
        Ok(None) => return Ok(false),
        Err(error) => return Err(format!("{error:#}")),
    };
    let mut errors = Vec::new();
    let mut failed = None;
    let ok = Api::call(&mut client, request, |response, _| match response {
        // alloc: error path, the daemon's errors, said once it is done
        ResponseRef::Error(error) => errors.push(error.what.to_owned()),
        response => {
            if failed.is_none()
                && let Err(why) = on(response)
            {
                failed = Some(why);
            }
        }
    })
    .map_err(|error| error.to_string())?;
    if let Some(why) = failed {
        return Err(why);
    }
    match (ok, errors.is_empty()) {
        (true, true) => Ok(true),
        (_, false) => Err(errors.join("\n")),
        (false, true) => Err("the daemon answered not ok and said no error".to_owned()),
    }
}

/// `blaze debug profile [--seconds N]`: the last N seconds (60) of the daemon's flight
/// recorder as a Chrome trace in its state directory, whose path is printed.
fn profile(words: Vec<OsString>) -> Result<(), Refusal> {
    let mut seconds = 60u32;
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        let value = match word.strip_prefix("--seconds") {
            Some("") => words.next(),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        seconds = value.and_then(|it| it.parse().ok()).ok_or_else(|| {
            Refusal::Usage(format!("blaze debug profile takes `--seconds N`, not `{word}`"))
        })?;
    }
    let mut trace = None;
    ask(Request::Profile { seconds }, Start::Never, |response| {
        if let ResponseRef::Profiled { trace: it } = response {
            trace = Some(it.to_vec());
        }
        Ok(())
    })?;
    let trace = trace
        .ok_or_else(|| "the daemon answered `blaze debug profile` with no trace".to_owned())?;
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_secs());
    let path = state.join(format!("profile-{now}.json"));
    std::fs::write(&path, trace).map_err(|error| format!("writing {}: {error}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

/// `blaze debug live`: the daemon's threads as they run, redrawn with each frame, until
/// Ctrl-C.
fn watch() -> Result<(), Refusal> {
    let width = super::place::stderr_columns().map_or(100, usize::from);
    let mut top = live::Top::default();
    let mut text = String::new();
    // Each table is one stream across the frames: its decoder keeps the schema and dictionaries.
    let mut decoders = [&schema::SPANS, &schema::COUNTERS, &schema::OPEN].map(live::Decoder::new);
    let mut tables: [Vec<RecordBatch>; 3] = Default::default();
    ask(Request::Live, Start::Never, |response| {
        let ResponseRef::Live(frame) = response else { return Ok(()) };
        for ((decoder, bytes), batches) in
            decoders.iter_mut().zip([frame.spans, frame.counters, frame.open]).zip(&mut tables)
        {
            decoder.decode(bytes, batches).map_err(|error| format!("the live feed: {error}"))?;
        }
        let [spans, counters, open] = &tables;
        top.take(frame.at_ns, frame.skipped, frame.lost, spans, counters, open)
            .map_err(|error| format!("the live feed: {error}"))?;
        top.render(width, &mut text);
        let mut out = std::io::stdout().lock();
        write!(out, "\x1b[2J\x1b[H{text}")
            .and_then(|()| out.flush())
            .map_err(|error| format!("the terminal: {error}"))
    })?;
    Ok(())
}

/// How `blaze debug sql` and `blaze explain` print their answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Table,
    /// A JSON object a row, a line each, its columns in order.
    Json,
}

/// `blaze debug sql [--json] QUERY`: over every row the daemon recorded before it was asked
/// ([`history_source`]). `--live` and `--last` are gone (2026-11-04): the plain query sees what
/// they showed, with no row read twice.
fn query(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut shape, mut sql) = (Shape::Table, None);
    for word in texts(words)? {
        match word.as_str() {
            "--json" => shape = Shape::Json,
            flag if flag.starts_with("--") => {
                return Err(Refusal::Usage(format!("{DEBUG_USAGE}, not `{flag}`")));
            }
            _ if sql.is_some() => {
                return Err(Refusal::Usage(format!("{DEBUG_USAGE}: one query, quoted")));
            }
            _ => sql = Some(word),
        }
    }
    let sql = sql.ok_or_else(|| Refusal::Usage(DEBUG_USAGE.to_owned()))?;
    print(&run(&sql)?, shape)
}

fn explained(words: Vec<OsString>) -> Result<(), Refusal> {
    let words = texts(words)?;
    let json_asked = words.iter().skip(1).any(|it| it == "--json");
    let mut words = words.into_iter();
    let name = words.next().ok_or_else(|| {
        let names: Vec<_> = views::VIEWS.iter().map(|it| it.name).collect();
        let rules: Vec<_> = blaze_report::rules::RULES.iter().map(|it| it.id).collect();
        Refusal::Usage(format!(
            "{EXPLAIN_USAGE}; the rules are {}; the views are {}",
            rules.join(", "),
            names.join(", ")
        ))
    })?;
    if let Some(rule) = blaze_report::rules::find(&name) {
        let mut text = String::new();
        blaze_report::rules::explain_into(&mut text, rule);
        return stdout(&text);
    }
    if name == "memory" {
        return memory(words);
    }
    if !views::VIEWS.iter().any(|it| it.name == name)
        && let Some(report) = failure_of(&name)?
    {
        return stdout(&failure_text(&report, json_asked));
    }
    let view = views::find(&name).map_err(|error| error.to_string())?;
    let mut shape = Shape::Table;
    let mut args = Map::new();
    for word in words {
        if word == "--json" {
            shape = Shape::Json;
            continue;
        }
        let Some((key, value)) = word.split_once('=') else {
            return Err(Refusal::Usage(format!("{EXPLAIN_USAGE}, not `{word}`")));
        };
        args.insert(key.to_owned(), argument(view, key, value)?);
    }
    let sql = views::render(view, &args).map_err(|error| error.to_string())?;
    print(&run(&sql)?, shape)
}

/// `blaze explain memory [BUILD] [limit=N] [--json]`: who owned a build's memory, from its
/// profile alone (docs/memory-footprint.md, step 1): the timeline per minute (footprint, live
/// heap, blaze-mem, kept), the tasks with the largest peaks with their crate and unit, the
/// breakdown by tag at the footprint's peak, and the call sites that allocated the most. Each
/// part is a view of its own (`memory_timeline`, `memory_peak_tasks`, `memory_at_peak`,
/// `memory_sites`); the build is the last one unless named. With `--json`, a line a row, each
/// with its `part`.
fn memory(words: impl Iterator<Item = String>) -> Result<(), Refusal> {
    let (mut build, mut limit, mut json) = (None, None, false);
    for word in words {
        match (word.as_str(), word.split_once('=')) {
            ("--json", _) => json = true,
            (_, Some(("build", value))) | (value, None) if build.is_none() => {
                let it = value.parse::<u64>().map_err(|_| {
                    Refusal::Usage(format!("{MEMORY_USAGE}: the build is a number, not `{value}`"))
                })?;
                build = Some(it);
            }
            (_, Some(("limit", value))) if limit.is_none() => {
                let it = value.parse::<u64>().map_err(|_| {
                    Refusal::Usage(format!("{MEMORY_USAGE}: the limit is a number, not `{value}`"))
                })?;
                limit = Some(it);
            }
            _ => return Err(Refusal::Usage(format!("{MEMORY_USAGE}, not `{word}`"))),
        }
    }
    let mut out = String::new();
    for (part, name, heading) in MEMORY_PARTS {
        let view = views::find(name).map_err(|error| error.to_string())?;
        let mut args = Map::new();
        if let Some(build) = build {
            args.insert("build".to_owned(), Value::from(build));
        }
        if let Some(limit) = limit
            && view.args.iter().any(|it| it.name == "limit")
        {
            args.insert("limit".to_owned(), Value::from(limit));
        }
        let sql = views::render(view, &args).map_err(|error| error.to_string())?;
        let batches = run(&sql)?;
        if json {
            let rows = self::json(&batches)?;
            for line in rows.lines() {
                // Each row an object: its part first, then its columns.
                let columns = line.strip_prefix('{').unwrap_or(line);
                let sep = if columns.starts_with('}') { "" } else { "," };
                _ = writeln!(out, "{{\"part\":\"{part}\"{sep}{columns}");
            }
        } else {
            _ = writeln!(out, "{heading} ({name}):");
            out.push_str(&table(&batches)?);
            out.push('\n');
        }
    }
    stdout(&out)
}

/// The file the CLI keeps the last build's failures in, for `blaze explain PACKAGE`: a JSON
/// array of reports (`blaze_report::Report`), in the daemon's state directory.
pub fn last_failure() -> Result<PathBuf, String> {
    crate::serve::state()
        .map(|it| it.join("last-failure.json"))
        .map_err(|error| format!("{error:#}"))
}

/// `failures` kept in `file` whole: written beside it, then moved over it.
pub fn remember(file: &Path, failures: &[blaze_api::Report]) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("{}: {error}", file.display());
    let text = serde_json::to_string(failures).map_err(|error| failed(&error))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|error| failed(&error))?;
    }
    let part = file.with_extension("json.part");
    std::fs::write(&part, text)
        .and_then(|()| std::fs::rename(&part, file))
        .map_err(|error| failed(&error))
}

/// The last build's failure of package `name` (the package its report is of,
/// [`blaze_api::Report::subject`]); `None` when it had none, or no build failed yet.
fn failure_of(name: &str) -> Result<Option<blaze_api::Report>, Refusal> {
    let file = last_failure()?;
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::Failed(format!("{}: {error}", file.display()))),
    };
    let reports: Vec<blaze_api::Report> = serde_json::from_str(&text)
        .map_err(|error| format!("{}: not the reports blaze keeps: {error}", file.display()))?;
    Ok(reports.into_iter().find(|it| it.subject() == Some(name)))
}

/// A failure as `blaze explain PACKAGE` prints it: the report, then every package it blocks;
/// with `--json`, the report whole.
fn failure_text(report: &blaze_api::Report, json: bool) -> String {
    if json {
        let mut text = blaze_report::json(report);
        text.push('\n');
        return text;
    }
    let mut text =
        blaze_report::render(report, blaze_report::Caps::PLAIN, &blaze_report::Roots::NONE);
    if let Some(blocks) = &report.blocks {
        // Writing to a String cannot fail.
        let _ = writeln!(text, "\nblocks {} {}:", blocks.count, blocks.what);
        for name in &blocks.all {
            let _ = writeln!(text, "  {name}");
        }
    }
    text
}

/// `text` on stdout.
fn stdout(text: &str) -> Result<(), Refusal> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

/// `value` as the JSON of argument `key`'s type; a key the view does not take is passed as
/// text, for the view to refuse by name.
fn argument(view: &views::View, key: &str, value: &str) -> Result<Value, Refusal> {
    let Some(arg) = view.args.iter().find(|it| it.name == key) else {
        return Ok(Value::from(value));
    };
    let wrong = || {
        Refusal::Usage(format!(
            "{}'s `{key}` is a {}, and `{value}` is none",
            view.name,
            arg.ty.name()
        ))
    };
    Ok(match arg.ty {
        views::ArgType::U64 => Value::from(value.parse::<u64>().map_err(|_| wrong())?),
        views::ArgType::I64 => Value::from(value.parse::<i64>().map_err(|_| wrong())?),
        views::ArgType::F64 => serde_json::Number::from_f64(value.parse().map_err(|_| wrong())?)
            .map(Value::Number)
            .ok_or_else(wrong)?,
        views::ArgType::Text => Value::from(value),
    })
}

/// `sql` over the history in the daemon's state directory, by the DuckDB `duckdb.rs`
/// finds.
fn run(sql: &str) -> Result<Vec<RecordBatch>, Refusal> {
    let (duckdb, dir) = history_source()?;
    let query = sql::Query { duckdb: &duckdb, history: &dir, scope: sql::Scope::All, sql };
    Ok(sql::run(&query).map_err(|error| error.to_string())?)
}

/// The DuckDB that reads the history (`BLAZE_DUCKDB`'s, or blaze's pin, fetched on first
/// use: `duckdb.rs`), and the history in the daemon's state directory, flushed: what every reader of the history (`blaze debug sql`, `blaze explain`, `blaze
/// slow`, `blaze debug time`) reads through.
///
/// A daemon that writes the history ([`history::writer`]) holds rows it has recorded and not
/// yet flushed (up to [`history::EVERY`] of them). It is asked to flush ([`Request::Flush`])
/// and answers with the flush's number once the flush committed; the manifest read after says
/// that flush or a later one ([`history::Manifest::flushed`]), so the reader reads every row
/// recorded before it asked, each once: a flush moves rows from the recorder to a file, and
/// nothing is in both. With no daemon writing it, the rows a dead daemon had not flushed are
/// written to files first ([`history::recover_dead`]), so every row is in a file. A daemon
/// that writes the history and can't be asked, or whose flush the manifest does not show, is
/// an error naming it: never a read that quietly misses rows.
fn history_source() -> Result<(PathBuf, PathBuf), Refusal> {
    Ok(history_paths()?)
}

/// The DuckDB that reads the history, and the history's directory: an error names what is
/// missing.
pub(super) fn history_paths() -> Result<(PathBuf, PathBuf), String> {
    let duckdb = super::duckdb::path()?;
    let state: PathBuf = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    let dir = history::dir_in(&state);
    if let Some(pid) = history::writer(&dir) {
        let mut flushed = None;
        ask(Request::Flush, Start::Never, |response| {
            if let ResponseRef::Flushed { seq } = response {
                flushed = Some(seq);
            }
            Ok(())
        })
        .map_err(|error| format!("the daemon (pid {pid}) writes the profile history {}, and asking it to flush failed: {error}", dir.display()))?;
        let seq = flushed.ok_or_else(|| {
            format!(
                "the daemon (pid {pid}) answered the ask to flush the profile history with no flush"
            )
        })?;
        let manifest = history::read_manifest(&dir).map_err(|error| error.to_string())?;
        if manifest.flushed() < seq {
            return Err(format!(
                "the daemon (pid {pid}) said its flush {seq} of the profile history {} committed, and the manifest says flush {}",
                dir.display(),
                manifest.flushed()
            ));
        }
    } else {
        // A daemon that died left its last rows in its flight recorder's mapping: written to
        // files and listed before the read, never a read that quietly misses them.
        history::recover_dead(&dir).map_err(|error| {
            format!("writing the rows a dead daemon left in the profile history {}: {error}", dir.display())
        })?;
    }
    Ok((duckdb, dir))
}

fn print(batches: &[RecordBatch], shape: Shape) -> Result<(), Refusal> {
    let text = match shape {
        Shape::Table => table(batches)?,
        Shape::Json => json(batches)?,
    };
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

/// The batches as a table: a header of the columns' names, a rule, a line a row, then the
/// count of rows.
fn table(batches: &[RecordBatch]) -> Result<String, String> {
    let Some(first) = batches.first() else { return Ok("0 rows\n".to_owned()) };
    let names: Vec<String> =
        first.schema().fields().iter().map(|it| it.name().to_owned()).collect();
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let cells =
                batch.columns().iter().map(|column| cell(column.as_ref(), row).map(|it| text(&it)));
            rows.push(cells.collect::<Result<Vec<_>, _>>()?);
        }
    }
    let mut widths: Vec<usize> = names.iter().map(|it| it.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        for (at, (cell, &width)) in cells.iter().zip(&widths).enumerate() {
            let sep = if at == 0 { "" } else { "  " };
            _ = write!(out, "{sep}{cell:<width$}");
        }
        let kept = out.trim_end_matches(' ').len();
        out.truncate(kept);
        out.push('\n');
    };
    line(&mut out, &names);
    let rule: Vec<String> = widths.iter().map(|it| "-".repeat(*it)).collect();
    line(&mut out, &rule);
    for row in &rows {
        line(&mut out, row);
    }
    _ = writeln!(out, "{} rows", rows.len());
    Ok(out)
}

/// The batches as JSON lines: an object a row, its columns in the query's order.
fn json(batches: &[RecordBatch]) -> Result<String, String> {
    let mut out = String::new();
    for batch in batches {
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            out.push('{');
            for (at, (field, column)) in schema.fields().iter().zip(batch.columns()).enumerate() {
                if at > 0 {
                    out.push(',');
                }
                let key = serde_json::to_string(field.name()).map_err(|error| error.to_string())?;
                let value = cell(column.as_ref(), row)?;
                _ = write!(out, "{key}:{value}");
            }
            out.push_str("}\n");
        }
    }
    Ok(out)
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(it) => it.clone(), // clone: a cell, printed once
        it => it.to_string(),
    }
}

/// Row `row` of `array` as JSON. A column of a type this does not print is an error naming it.
// dyn: a column of a query's answer, read once a cell
fn cell(array: &dyn Array, row: usize) -> Result<Value, String> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    macro_rules! primitive {
        ($($ty:ty),*) => {
            $(if let Some(it) = array.as_primitive_opt::<$ty>() {
                return Ok(Value::from(it.value(row)));
            })*
        };
    }
    primitive!(
        types::Int8Type,
        types::Int16Type,
        types::Int32Type,
        types::Int64Type,
        types::UInt8Type,
        types::UInt16Type,
        types::UInt32Type,
        types::UInt64Type,
        types::Float32Type,
        types::Float64Type,
        types::TimestampSecondType,
        types::TimestampMillisecondType,
        types::TimestampMicrosecondType,
        types::TimestampNanosecondType,
        types::Date32Type,
        types::Date64Type,
        types::DurationSecondType,
        types::DurationMillisecondType,
        types::DurationMicrosecondType,
        types::DurationNanosecondType
    );
    if let Some(it) = array.as_string_opt::<i32>() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_string_opt::<i64>() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_string_view_opt() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_boolean_opt() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(dictionary) = array.as_any_dictionary_opt() {
        let key = dictionary.normalized_keys()[row];
        return cell(dictionary.values().as_ref(), key);
    }
    Err(format!(
        "a column of type {} can't be printed: cast it in the query (`::VARCHAR`)",
        array.data_type()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, DictionaryArray, Float64Array, Int64Array, StringArray};

    use super::*;

    fn batch() -> RecordBatch {
        let names: DictionaryArray<types::Int32Type> =
            vec![Some("parse"), None, Some("parse")].into_iter().collect();
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("name", Arc::new(names)),
            ("count", Arc::new(Int64Array::from(vec![3, 10, -1]))),
            ("share", Arc::new(Float64Array::from(vec![0.5, 1.0, 0.25]))),
            ("note", Arc::new(StringArray::from(vec![Some("a \"quoted\" one"), Some(""), None]))),
        ];
        RecordBatch::try_from_iter(columns).unwrap()
    }

    #[test]
    fn a_table_lines_up_its_columns_and_counts_its_rows() {
        let table = table(&[batch()]).unwrap();
        assert_eq!(
            table,
            "name   count  share  note\n\
             -----  -----  -----  --------------\n\
             parse  3      0.5    a \"quoted\" one\n\
             \x20      10     1.0\n\
             parse  -1     0.25\n\
             3 rows\n"
        );
        assert_eq!(super::table(&[]).unwrap(), "0 rows\n");
    }

    #[test]
    fn json_is_an_object_a_row_in_the_columns_order() {
        let json = json(&[batch()]).unwrap();
        let lines: Vec<&str> = json.lines().collect();
        assert_eq!(lines[0], r#"{"name":"parse","count":3,"share":0.5,"note":"a \"quoted\" one"}"#);
        assert_eq!(lines[1], r#"{"name":null,"count":10,"share":1.0,"note":""}"#);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn an_argument_is_read_as_its_views_type() {
        let view = views::find("slowest_queries").unwrap();
        assert_eq!(argument(view, "limit", "5").ok(), Some(Value::from(5u64)));
        assert!(
            matches!(argument(view, "limit", "five"), Err(Refusal::Usage(it)) if it.contains("`limit` is a u64"))
        );
        assert_eq!(
            argument(view, "nope", "x").ok(),
            Some(Value::from("x")),
            "left for the view to refuse"
        );
    }
}
