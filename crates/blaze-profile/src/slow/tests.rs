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

fn span(thread: u32, span_id: u64, parent: Option<u64>, name: &str, start: i64, dur: i64, job: u64) -> SpanIn {
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
        start_ns: start * U,
        dur_ns: dur * U,
        self_cpu_ns: None,
    }
}

fn workers(threads: impl IntoIterator<Item = u32>) -> Facts {
    let threads = threads.into_iter().map(|thread| ThreadName { process: P, thread, name: format!("blaze-work-{thread}") }).collect();
    Facts { threads, ..Facts::default() }
}

fn report(switches: Vec<SwitchIn>, spans: Vec<SpanIn>, wall: i64, facts: &Facts) -> Report {
    let model = Model::build(Input { build: 1, build_row: Some((0, wall * U)), switches, spans, events: Vec::new() }).unwrap();
    let path = path::critical(&model);
    let one = analyse(&model, &path, facts);
    assert_eq!(one, analyse(&model, &path, facts), "the same rows give the same report");
    assert!(one.findings.windows(2).all(|it| it[0].saves_ns >= it[1].saves_ns || it[1].estimate == Estimate::Unknown), "ranked by saving");
    one
}

fn finding(report: &Report, kind: Kind) -> &Finding {
    report.findings.iter().find(|it| it.kind == kind).unwrap_or_else(|| panic!("no {kind:?} finding in {report:#?}"))
}

/// Four workers; job 1 runs alone in `impls publish` from 0 to 100 while the other three finish
/// at 10: 1.3 workers are busy on average along the path, so parallelising it would save
/// 100 x (1 - 1.3/4) = 67.5 units.
#[test]
fn serial_work_on_the_critical_path_is_charged_its_time_times_the_idle_share_of_workers() {
    let mut switches = vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)];
    for it in 2..=4 {
        switches.extend([switch(it, 0, Event::Started, u64::from(it)), switch(it, 10, Event::Ended, u64::from(it))]);
    }
    let mut publish = span(1, 1, None, "impls publish", 0, 100, 1);
    publish.krate = Some("core".into());
    let it = report(switches, vec![publish], 100, &workers(1..=4));
    let serial = finding(&it, Kind::Serial);
    assert_eq!((serial.subject.as_str(), serial.fix, serial.estimate), ("impls publish", Fix::Parallelise, Estimate::CriticalPath));
    assert_eq!(serial.saves_ns, 675 * U / 10);
    assert_eq!(it.summary.workers, 4);
    assert_eq!(it.summary.path_ns, 100 * U);
}

/// The same span counting a tenth of its running time as CPU is starved, not serial: more
/// workers would not help, CPU would.
#[test]
fn a_path_span_that_got_little_cpu_for_its_running_is_starved_not_serial() {
    let mut publish = span(1, 1, None, "impls publish", 0, 100, 1);
    publish.self_cpu_ns = Some(10 * U);
    let it = report(vec![switch(1, 0, Event::Started, 1), switch(1, 100, Event::Ended, 1)], vec![publish], 100, &workers(1..=4));
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
    assert!(wait.evidence.iter().any(|it| it.contains("woken by: build script proof serde")), "{:?}", wait.evidence);
    let serial = finding(&it, Kind::Serial);
    assert_eq!(serial.subject, "build script proof", "the owner's run stands on the path in the wait's place");
}

/// Four workers run the whole build; three count a full CPU per running second, one a tenth,
/// and the critical path runs on it.
#[test]
fn a_worker_given_a_fraction_of_the_others_cpu_rate_is_starved_and_charged_its_path_time() {
    let mut switches = Vec::new();
    let mut spans = Vec::new();
    for it in 1..=4u32 {
        switches.extend([switch(it, 0, Event::Started, u64::from(it)), switch(it, 100, Event::Ended, u64::from(it))]);
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
        switches.push(switch(thread, i as i64 + 1, Event::Parked { on: WaitedOn::Key, value: 9 }, job));
    }
    let it = report(switches, Vec::new(), 100, &workers(10..=13));
    let unended = it.blind_spots.iter().find(|it| it.kind == BlindKind::UnendedWait).unwrap_or_else(|| panic!("{:#?}", it.blind_spots));
    assert!(unended.title.starts_with("50 waits never end in the record"), "{}", unended.title);
    assert!(unended.hidden_ns <= 100 * U, "the wall the waits cover, not their sum: {}", unended.hidden_ns);
    assert!(it.findings.iter().all(|it| it.kind != Kind::Wait), "no wait class holds them: {:#?}", it.findings);
    assert!(it.blind_spots.iter().all(|it| it.kind != BlindKind::UnnamedWait), "{:#?}", it.blind_spots);
}
