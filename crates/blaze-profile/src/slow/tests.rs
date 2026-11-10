//! Fixtures for the analysis's hard logic: what the critical path charges to whom.

use super::*;
use crate::{
    flight::WaitedOn,
    time::{Event, Input, SpanIn, SwitchIn, path},
};

const P: u32 = 7;
/// One unit of fixture time: 10 ms, so a fixture of 100 units is a one-second build.
const U: i64 = 10_000_000;

fn switch(thread: u32, t: i64, event: Event, job: u64) -> SwitchIn {
    SwitchIn::of(P, thread, t * U, event, job, job as u32, thread)
}

fn span(
    thread: u32,
    span_id: u64,
    parent: Option<u64>,
    name: &str,
    start: i64,
    dur: i64,
    job: u64,
) -> SpanIn {
    SpanIn {
        process: P,
        thread,
        span_id,
        parent,
        depth: u16::from(parent.is_some()),
        name: name.to_owned(),
        krate: None,
        function: None,
        unit: None,
        awaits: None,
        owner: None,
        job: Some(job),
        creator: None,
        start_ns: start * U,
        dur_ns: dur * U,
        self_cpu_ns: None,
        folded_ns: 0,
    }
}

fn workers(threads: impl IntoIterator<Item = u32>) -> Facts {
    let threads = threads
        .into_iter()
        .map(|thread| ThreadName { process: P, thread, name: format!("blaze-work-{thread}") })
        .collect();
    Facts { threads, ..Facts::default() }
}

fn report(switches: Vec<SwitchIn>, spans: Vec<SpanIn>, wall: i64, facts: &Facts) -> Report {
    // The threads' CPU as `load::facts` counts it in DuckDB, over the fixture's spans.
    let mut facts = facts.clone();
    for it in &spans {
        let at = match facts
            .thread_cpu
            .iter()
            .position(|cpu| (cpu.process, cpu.thread) == (it.process, it.thread))
        {
            Some(at) => at,
            None => {
                facts.thread_cpu.push(ThreadCpu {
                    process: it.process,
                    thread: it.thread,
                    ..ThreadCpu::default()
                });
                facts.thread_cpu.len() - 1
            }
        };
        let cpu = &mut facts.thread_cpu[at];
        match it.self_cpu_ns {
            Some(ns) => (cpu.cpu_ns, cpu.counted) = (cpu.cpu_ns + ns, cpu.counted + 1),
            None => cpu.uncounted += 1,
        }
    }
    let facts = &facts;
    let model = Model::build(Input {
        build: 1,
        build_row: Some((0, wall * U)),
        switches,
        spans,
        events: Vec::new(),
    })
    .unwrap();
    let path = path::critical(&model);
    let one = analyse(&model, &path, facts);
    assert_eq!(one, analyse(&model, &path, facts), "the same rows give the same report");
    assert!(
        one.findings
            .windows(2)
            .all(|it| it[0].saves_ns >= it[1].saves_ns || it[1].estimate == Estimate::Unknown),
        "ranked by saving"
    );
    one
}

fn finding(report: &Report, kind: Kind) -> &Finding {
    report
        .findings
        .iter()
        .find(|it| it.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} finding in {report:#?}"))
}

/// Four workers; job 1 runs alone in `impls publish` from 0 to 100 while the other three finish
/// at 10: 1.3 workers are busy on average along the path, so parallelising it would save
/// 100 x (1 - 1.3/4) = 67.5 units.
#[test]
fn serial_work_on_the_critical_path_is_charged_its_time_times_the_idle_share_of_workers() {
    let mut switches = vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)];
    for it in 2..=4 {
        switches.extend([
            switch(it, 0, Event::Started, u64::from(it)),
            switch(it, 10, Event::Ended, u64::from(it)),
        ]);
    }
    let mut publish = span(1, 1, None, "impls publish", 0, 100, 1);
    publish.krate = Some("core".into());
    let it = report(switches, vec![publish], 100, &workers(1..=4));
    let serial = finding(&it, Kind::Serial);
    assert_eq!(
        (serial.subject.as_str(), serial.fix, serial.estimate),
        ("impls publish", Fix::Parallelise, Estimate::CriticalPath)
    );
    assert_eq!(serial.saves_ns, 675 * U / 10);
    assert_eq!(it.summary.workers, 4);
    assert_eq!(it.summary.path_ns, 100 * U);
}

/// A finding about a span name carries the commonest stacks sampled inside those spans, on
/// and off the CPU, at most [`STACKS`] of each; the text shows each innermost first.
#[test]
fn a_finding_about_a_span_carries_its_top_sampled_stacks_on_and_off_the_cpu() {
    let mut switches = vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)];
    for it in 2..=4 {
        switches.extend([
            switch(it, 0, Event::Started, u64::from(it)),
            switch(it, 10, Event::Ended, u64::from(it)),
        ]);
    }
    let stack = |name: &str, on_cpu: bool, frames: &[&str], samples: u64| SpanStack {
        name: name.to_owned(),
        on_cpu,
        frames: frames.iter().map(|it| (*it).to_owned()).collect(),
        samples,
    };
    let mut facts = workers(1..=4);
    facts.span_stacks = vec![
        stack("impls publish", true, &["main", "publish", "hash_impls"], 40),
        stack("impls publish", true, &["main", "publish", "sort_impls"], 30),
        stack("impls publish", true, &["main", "publish", "alloc"], 20),
        stack("impls publish", true, &["main", "publish", "free"], 10),
        stack("impls publish", false, &["main", "publish", "futex_wait"], 5),
        stack("other span", true, &["main", "other"], 99),
    ];
    let it = report(switches, vec![span(1, 1, None, "impls publish", 0, 100, 1)], 100, &facts);
    let serial = finding(&it, Kind::Serial);
    let tops: Vec<(&str, u64)> = serial
        .stacks
        .on_cpu
        .iter()
        .map(|it| (it.frames.last().unwrap().as_str(), it.samples))
        .collect();
    assert_eq!(
        tops,
        [("hash_impls", 40), ("sort_impls", 30), ("alloc", 20)],
        "the top {STACKS}, most first"
    );
    assert_eq!(serial.stacks.off_cpu.len(), 1);
    assert_eq!(serial.stacks.off_cpu[0].frames.last().map(String::as_str), Some("futex_wait"));
    let said = text(&it, 10);
    assert!(
        said.contains("sampled on the CPU inside it 40x: hash_impls < publish < main"),
        "{said}"
    );
    assert!(said.contains("sampled blocked inside it 5x: futex_wait < publish < main"), "{said}");
    assert!(!said.contains("other < main"), "another span's stacks are not this finding's: {said}");
    let json = serde_json::to_value(serial).unwrap();
    assert_eq!(json["stacks"]["on_cpu"][0]["samples"], 40);
    let unsampled = report(
        vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)],
        vec![span(1, 1, None, "x", 0, 100, 1)],
        100,
        &workers(1..=1),
    );
    for finding in &unsampled.findings {
        assert!(
            serde_json::to_value(finding).unwrap().get("stacks").is_none(),
            "no stacks, no field"
        );
    }
}

/// The same span counting a tenth of its running time as CPU is starved, not serial: more
/// workers would not help, CPU would.
#[test]
fn a_path_span_that_got_little_cpu_for_its_running_is_starved_not_serial() {
    let mut publish = span(1, 1, None, "impls publish", 0, 100, 1);
    publish.self_cpu_ns = Some(10 * U);
    let it = report(
        vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)],
        vec![publish],
        100,
        &workers(1..=4),
    );
    let starved = finding(&it, Kind::Starved);
    assert_eq!((starved.subject.as_str(), starved.fix), ("impls publish", Fix::GiveCpu));
    assert_eq!(starved.saves_ns, 90 * U);
    assert!(it.findings.iter().all(|it| it.kind != Kind::Serial));
}

/// Job 2 parks at 21 on a key job 1 runs, inside a `wait` span naming `summary_in(serde)`; job 1
/// runs `build script proof` to 60 and the wait ends. The path crosses the wait to job 1, so the
/// wait held the build up for its 39 units, and its owner is what job 1 was doing.
#[test]
fn a_crossed_wait_is_charged_its_time_on_the_path_and_names_what_its_owner_was_running() {
    let (a, b) = (1 << 32 | 1, 1 << 32 | 2);
    let mut proof = span(10, 1, None, "build script proof", 0, 60, a);
    proof.function = Some("serde".into());
    let unit = span(11, 2, None, "unit serde_derive", 10, 90, b);
    let mut wait = span(11, 3, Some(2), WAIT_SPAN, 20, 40, b);
    wait.awaits = Some("summary_in(serde)".into());
    wait.owner = Some(a);
    let it = report(
        vec![
            switch(10, 0, Event::Started, a),
            switch(11, 10, Event::Started, b),
            switch(11, 21, Event::Parked { on: WaitedOn::Key, value: 5 }, b),
            switch(10, 60, Event::Ended, a),
            switch(11, 60, Event::Resumed, b),
            switch(11, 100, Event::Ended, b),
        ],
        vec![proof, unit, wait],
        100,
        &workers(10..=13),
    );
    let wait = finding(&it, Kind::Wait);
    assert_eq!((wait.subject.as_str(), wait.estimate), ("key summary_in", Estimate::CriticalPath));
    assert_eq!(wait.saves_ns, 39 * U);
    assert!(
        wait.evidence.iter().any(|it| it.contains("woken by: build script proof serde")),
        "{:?}",
        wait.evidence
    );
    let serial = finding(&it, Kind::Serial);
    assert_eq!(
        serial.subject, "build script proof",
        "the owner's run stands on the path in the wait's place"
    );
}

/// Four workers run the whole build; three count a full CPU per running second, one a tenth,
/// and the critical path runs on it.
#[test]
fn a_worker_given_a_fraction_of_the_others_cpu_rate_is_starved_and_charged_its_path_time() {
    let mut switches = Vec::new();
    let mut spans = Vec::new();
    for it in 1..=4u32 {
        switches.extend([
            switch(it, 0, Event::Started, u64::from(it)),
            switch(it, 100, Event::Ended, u64::from(it)),
        ]);
        let mut job = span(it, u64::from(it), None, "job", 0, 100, u64::from(it));
        job.self_cpu_ns = Some(if it == 1 { 10 * U } else { 100 * U });
        spans.push(job);
    }
    let it = report(switches, spans, 100, &workers(1..=4));
    let starved = finding(&it, Kind::StarvedWorkers);
    assert!(starved.evidence[0].starts_with("1 of 4 workers"), "{:?}", starved.evidence);
    assert!(starved.evidence[0].contains("blaze-work-1"), "{:?}", starved.evidence);
    assert_eq!(starved.saves_ns, 90 * U);
}

/// Many jobs parked on one key the record never resumes: their parked time, each to the
/// record's end, is told apart as waits that never end, its size the wall they cover, never
/// summed into a wait class of millions of seconds.
#[test]
fn waits_the_record_never_ends_are_told_apart_by_the_wall_they_cover() {
    let jobs: Vec<u64> = (1..=50).map(|it| 1 << 32 | it).collect();
    let mut switches = Vec::new();
    for (i, &job) in jobs.iter().enumerate() {
        let thread = 10 + (i % 4) as u32;
        switches.push(switch(thread, i as i64, Event::Started, job));
        switches.push(switch(
            thread,
            i as i64 + 1,
            Event::Parked { on: WaitedOn::Key, value: 9 },
            job,
        ));
    }
    let it = report(switches, Vec::new(), 100, &workers(10..=13));
    let unended = it
        .blind_spots
        .iter()
        .find(|it| it.kind == BlindKind::UnendedWait)
        .unwrap_or_else(|| panic!("{:#?}", it.blind_spots));
    assert!(unended.title.starts_with("50 waits never end in the record"), "{}", unended.title);
    assert!(
        unended.hidden_ns <= 100 * U,
        "the wall the waits cover, not their sum: {}",
        unended.hidden_ns
    );
    assert!(
        it.findings.iter().all(|it| it.kind != Kind::Wait),
        "no wait class holds them: {:#?}",
        it.findings
    );
    assert!(
        it.blind_spots.iter().all(|it| it.kind != BlindKind::UnnamedWait),
        "{:#?}",
        it.blind_spots
    );
}

/// The daemon of the [`a_build_whose_daemon_was_killed_is_reported_with_what_it_was_running`]
/// fixture, in a child process: it claims the history `dir` as blaze-cargo's daemon does,
/// starts build 1, closes one span in it, enters a wait it never leaves, flushes, says it is
/// ready, and sleeps until it is killed.
#[cfg(unix)]
fn killed_daemon(dir: &std::path::Path) -> ! {
    use std::io::Write as _;
    crate::install();
    let owner = std::fs::File::create(dir.join(crate::history::OWNER)).unwrap();
    owner.lock().unwrap();
    write!(&owner, "{}", std::process::id()).unwrap();
    let mut history =
        crate::history::History::open(dir, crate::history::CAP_BYTES, crate::sampler::unsampled())
            .unwrap();
    history.build_started(crate::history::BuildRow {
        build_id: 1,
        commit: None,
        workspace: "/work/app".into(),
        command: "build".into(),
        started_ns: crate::clock::mono_ns(),
        dur_ns: None,
        started_unix_ns: None,
        outcome: crate::history::Outcome::Running,
        hits: None,
        misses: None,
        joined: None,
        tree: None,
        blaze: None,
        client_ns: None,
        daemon_cpu_ns: None,
    });
    let entered = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("worker stuck".to_owned())
            .spawn_scoped(scope, || {
                let _build = crate::flight::building(1);
                drop(tracing::info_span!(target: "blaze", "unit done", krate = "app").entered());
                let _unit =
                    tracing::info_span!(target: "blaze", "unit bodies", krate = "app").entered();
                let awaits = crate::span::Name::of("the lock no one lets go");
                let on = blaze_graph::wait::WaitKind::Key;
                let _wait =
                    crate::span::Wait { awaits, function: awaits, owner: Some(7), on }.open();
                entered.wait();
                loop {
                    std::thread::park();
                }
            })
            .unwrap();
        entered.wait();
        // The history's drain thread takes the closed span within its period; flushed until a
        // flush holds it, at most 5 s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while history.flush(Some(1)).unwrap().spans == 0 {
            assert!(std::time::Instant::now() < deadline, "no flush held the closed span in 5 s");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::write(dir.join("ready"), b"").unwrap();
        loop {
            std::thread::park();
        }
    })
}

/// A daemon SIGKILLed mid-build leaves no end row, yet `blaze slow` reports its build: as ended
/// by the daemon's death, to its last row, naming the wait its stuck thread was inside and who
/// ran what it waited on. Remove the `open` reading from the flush, or the death from the build
/// pick, and this fails.
#[test]
#[cfg(unix)]
#[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
fn a_build_whose_daemon_was_killed_is_reported_with_what_it_was_running() {
    const CHILD: &str = "BLAZE_TEST_KILLED_DAEMON";
    if let Some(dir) = std::env::var_os(CHILD) {
        killed_daemon(std::path::Path::new(&dir));
    }
    let duckdb = std::path::PathBuf::from(
        std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
    );
    let dir = std::env::temp_dir().join(format!("blaze-slow-killed-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "slow::tests::a_build_whose_daemon_was_killed_is_reported_with_what_it_was_running",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD, &dir)
        .spawn()
        .unwrap();
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !dir.join("ready").exists() {
        assert!(child.try_wait().unwrap().is_none(), "the daemon child exited before it was ready");
        assert!(std::time::Instant::now() < deadline, "the daemon child was not ready in 60 s");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    let source = crate::time::load::Source { duckdb: &duckdb, history: &dir };
    let report = load::run(&source, None).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.build.id, 1);
    let died = report.build.died.as_ref().unwrap_or_else(|| {
        panic!("the build is reported as ended by its daemon's death: {report:?}")
    });
    assert_eq!(died.process, pid);
    assert!(died.read_ns.is_some(), "the flush wrote what the threads were inside");
    let stuck = died
        .threads
        .iter()
        .find(|it| it.name.as_deref() == Some("worker stuck"))
        .unwrap_or_else(|| panic!("{died:?}"));
    let names: Vec<&str> = stuck.spans.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["unit bodies", "wait"], "outermost first");
    assert_eq!(stuck.spans[1].awaits.as_deref(), Some("the lock no one lets go"));
    assert_eq!(stuck.spans[1].owner, Some(7));
    let text = text(&report, 10);
    let pid_said = format!("pid {pid}");
    for said in [
        "ended by the daemon's death",
        pid_said.as_str(),
        "worker stuck (tid ",
        ", awaiting the lock no one lets go, run by task 7 (",
    ] {
        assert!(text.contains(said), "the report says {said:?}:\n{text}");
    }
    _ = std::fs::remove_dir_all(&dir);
}

/// Runs `fixture`, DuckDB's SQL writing a history's Parquet files into `dir` (`{d}`), beside the
/// history's schema files.
fn history_of(duckdb: &std::path::Path, name: &str, fixture: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-slow-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    crate::history::History::open(&dir, crate::history::CAP_BYTES, crate::sampler::unsampled())
        .unwrap();
    let fixture = fixture.replace("{d}", &dir.display().to_string());
    let written = std::process::Command::new(duckdb)
        .args(["-batch", "-bail", "-init", "/dev/null", "-c", &fixture])
        .output()
        .unwrap();
    assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
    crate::history::list_written(&dir);
    dir
}

/// Two daemons in one history, pid 7 and then pid 8, each with a build 1 (as every daemon
/// numbered its builds before 2026-11-03 when the one before never flushed its build's end):
/// `blaze slow` refuses to read them as one build and names both, rather than sum their
/// workers and windows (selfbuild4 read a 200 s step as 2296 s of 72 workers). And a build of
/// one daemon takes no row of another process in its window.
#[test]
#[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
fn two_daemons_builds_numbered_alike_in_one_history_stay_apart() {
    let duckdb = std::path::PathBuf::from(
        std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
    );
    let builds = "COPY (SELECT * FROM (VALUES (1::UBIGINT, 1000::BIGINT, 100::BIGINT, 'ok', '/work', 'build')) \
        AS t(build_id, started_ns, dur_ns, outcome, workspace, command)) TO '{d}/1-00000002.builds.parquet';\n";
    let mixed = history_of(
        &duckdb,
        "mixed",
        &format!(
            "{builds}COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 1::UBIGINT, 0::USMALLINT, 'unit a', 0::BIGINT, 100::BIGINT), \
                (1, 8, 10, 1, 0, 'unit a', 1000, 100)) \
                AS t(build_id, process, thread, span_id, depth, name, start_ns, dur_ns)) TO '{{d}}/0-00000001.spans.parquet';\n\
             COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 0::BIGINT, 'started', 1::UBIGINT, 1::UINTEGER, 0::UINTEGER), \
                (1, 7, 10, 100, 'ended', 1, 1, 0), (1, 8, 10, 1000, 'started', 1, 1, 0), (1, 8, 10, 1100, 'ended', 1, 1, 0)) \
                AS t(build_id, process, thread, t_ns, event, job, fiber, core)) TO '{{d}}/0-00000001.switches.parquet';\n"
        ),
    );
    let source = crate::time::load::Source { duckdb: &duckdb, history: &mixed };
    match load::run(&source, Some(1)) {
        Err(crate::time::TimeError::ManyProcesses { build: 1, processes }) => {
            let pids: Vec<(u32, u64)> = processes.iter().map(|it| (it.process, it.rows)).collect();
            assert_eq!(pids, [(7, 3), (8, 3)], "each daemon named with its rows: {processes:?}");
        }
        other => panic!("two daemons' build 1 read as one build: {other:?}"),
    }
    _ = std::fs::remove_dir_all(&mixed);

    let apart = history_of(
        &duckdb,
        "apart",
        &format!(
            "{builds}COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 8::UINTEGER, 10::UINTEGER, 1::UBIGINT, 0::USMALLINT, 'unit a', 1000::BIGINT, 100::BIGINT), \
                (NULL, 7, 10, 2, 0, 'unit of no build', 1010, 50)) \
                AS t(build_id, process, thread, span_id, depth, name, start_ns, dur_ns)) TO '{{d}}/1-00000001.spans.parquet';\n\
             COPY (SELECT * FROM (VALUES (1::UBIGINT, 8::UINTEGER, 10::UINTEGER, 1000::BIGINT, 'started', 1::UBIGINT, 1::UINTEGER, 0::UINTEGER), \
                (1, 8, 10, 1100, 'ended', 1, 1, 0)) \
                AS t(build_id, process, thread, t_ns, event, job, fiber, core)) TO '{{d}}/1-00000001.switches.parquet';\n"
        ),
    );
    let source = crate::time::load::Source { duckdb: &duckdb, history: &apart };
    let pick = crate::time::load::pick(&source, Some(1)).unwrap_or_else(|error| panic!("{error}"));
    let crate::time::load::Rows { input, process, cut } =
        crate::time::load::rows(&source, pick).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((cut.floor_ns, cut.read), (0, 1), "a build within the budget is read whole");
    assert_eq!(process, Some(8));
    let processes: Vec<u32> = input.spans.iter().map(|it| it.process).collect();
    assert_eq!(
        processes,
        [8],
        "another process's row of no build in the window is not the build's"
    );
    load::run(&source, Some(1)).unwrap_or_else(|error| panic!("{error}"));
    _ = std::fs::remove_dir_all(&apart);
}

/// Every query `blaze slow` asks beside the time model (`load::facts`: the build's row, threads,
/// spans by name, misses, query counts, sampled stacks by span, threads' CPU and, with a window,
/// memory and the recorder) is one the pinned DuckDB binds and answers, with and without a
/// window; and the stacks are the [`STACKS`] commonest of each span name, a sample counted for
/// its span and that span's ancestors. DuckDB 1.5.5 refused the stacks' query ("Cannot mix
/// aggregates with non-aggregated columns"), so `blaze slow` failed on every build.
#[test]
#[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
fn every_query_of_slow_is_answered_and_the_stacks_are_the_commonest_of_each_span() {
    let duckdb = std::path::PathBuf::from(
        std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
    );
    // A root span and a leaf inside it. The leaf's samples: `a b` twice, four other stacks
    // once each, and one blocked; the root has one sample of its own.
    let fixture = "COPY (SELECT * FROM (VALUES (1::UBIGINT, 1000::BIGINT, 100::BIGINT, 'ok', '/work', 'build')) \
            AS t(build_id, started_ns, dur_ns, outcome, workspace, command)) TO '{d}/1-00000002.builds.parquet';\n\
         COPY (SELECT * FROM (VALUES \
            (1::UBIGINT, 8::UINTEGER, 10::UINTEGER, 1::UBIGINT, NULL::UBIGINT, 0::USMALLINT, 'root', 1000::BIGINT, 100::BIGINT), \
            (1, 8, 10, 2, 1, 1, 'leaf', 1010, 50)) \
            AS t(build_id, process, thread, span_id, parent, depth, name, start_ns, dur_ns)) TO '{d}/1-00000001.spans.parquet';\n\
         COPY (SELECT * FROM (VALUES \
            (1::UBIGINT, 8::UINTEGER, 10::UINTEGER, 1011::BIGINT, ['a', 'b'], 2::UBIGINT, 'on'), \
            (1, 8, 10, 1012, ['a', 'b'], 2, 'on'), (1, 8, 10, 1013, ['a', 'c'], 2, 'on'), \
            (1, 8, 10, 1014, ['a', 'd'], 2, 'on'), (1, 8, 10, 1015, ['a', 'e'], 2, 'on'), \
            (1, 8, 10, 1016, ['a', 'f'], 2, 'on'), (1, 8, 10, 1017, ['w'], 2, 'off'), \
            (1, 8, 10, 1070, ['a'], 1, 'on')) \
            AS t(build_id, process, thread, t_ns, stack, span_id, cpu)) TO '{d}/1-00000001.samples.parquet';\n\
         COPY (SELECT * FROM (VALUES \
            (NULL::UBIGINT, 8::UINTEGER, 'live bytes', 900::BIGINT, 5::DOUBLE), \
            (1, 8, 'live bytes', 1010, 7), (1, 8, 'live bytes', 1050, 9), \
            (NULL, 8, 'profile spans made', 900, 1), (1, 8, 'profile spans made', 1090, 3)) \
            AS t(build_id, process, name, t_ns, value)) TO '{d}/1-00000001.counters.parquet';\n";
    let dir = history_of(&duckdb, "facts", fixture);
    let source = crate::time::load::Source { duckdb: &duckdb, history: &dir };
    let stack = |name: &str, on_cpu, frames: &[&str], samples| SpanStack {
        name: name.to_owned(),
        on_cpu,
        frames: frames.iter().map(|it| (*it).to_owned()).collect(),
        samples,
    };
    let stacks = [
        stack("leaf", true, &["a", "b"], 2),
        stack("leaf", true, &["a", "c"], 1),
        stack("leaf", true, &["a", "d"], 1),
        stack("leaf", false, &["w"], 1),
        stack("root", true, &["a", "b"], 2),
        stack("root", true, &["a"], 1),
        stack("root", true, &["a", "c"], 1),
        stack("root", false, &["w"], 1),
    ];
    for window in [Some((1000, 1100)), None] {
        let facts = load::facts(&source, 1, window, Some(8))
            .unwrap_or_else(|error| panic!("window {window:?}: {error}"));
        assert_eq!(facts.build.outcome.as_deref(), Some("ok"), "window {window:?}");
        assert_eq!(facts.span_stacks, stacks, "window {window:?}");
        let kinds: Vec<(&str, u64)> =
            facts.span_kinds.iter().map(|it| (it.name.as_str(), it.runs)).collect();
        assert_eq!(kinds, [("leaf", 1), ("root", 1)], "window {window:?}");
        let cpu: Vec<(u32, u32, u64)> =
            facts.thread_cpu.iter().map(|it| (it.process, it.thread, it.uncounted)).collect();
        assert_eq!(cpu, [(8, 10, 2)], "window {window:?}");
        assert!(facts.misses.is_empty() && facts.query_counts.is_empty(), "window {window:?}");
        if window.is_some() {
            let memory = (facts.memory.live_before, facts.memory.live_first, facts.memory.live_peak);
            assert_eq!(memory, (Some(5), Some(7), Some(9)), "the window's memory");
            assert_eq!(facts.recorder.spans_made, Some(2), "the window's recorder");
        }
    }
    _ = std::fs::remove_dir_all(&dir);
}
