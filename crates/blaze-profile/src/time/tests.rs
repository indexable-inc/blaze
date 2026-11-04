use hegel::{TestCase, generators as gs};

use super::{
    path::{self, LinkKind, Reached},
    report::{self, By},
    *,
};

const P: u32 = 7;

fn switch(thread: u32, t_ns: i64, event: Event, job: u64, core: u32) -> SwitchIn {
    SwitchIn::of(P, thread, t_ns, event, job, job as u32, core)
}

/// `job` made runnable by `by`, on `by`'s thread.
fn ready(thread: u32, t_ns: i64, job: u64, by: Option<u64>, core: u32) -> SwitchIn {
    SwitchIn { by, ..switch(thread, t_ns, Event::Ready, job, core) }
}

/// `job` joining the graph at `t_ns`, having begun on its fiber at `since_ns` as `began_as`,
/// given to the pool by `by`.
fn started(
    thread: u32,
    t_ns: i64,
    job: u64,
    core: u32,
    since_ns: i64,
    began_as: u64,
    by: Option<u64>,
) -> SwitchIn {
    SwitchIn {
        by,
        since_ns: Some(since_ns),
        began_as: Some(began_as),
        ..switch(thread, t_ns, Event::Started, job, core)
    }
}

/// A `job` span: the root of a pool job's spans, naming it.
fn job_span(thread: u32, span_id: u64, job: u64, start_ns: i64, dur_ns: i64) -> SpanIn {
    SpanIn { job: Some(job), ..span(thread, span_id, None, "job", start_ns, dur_ns) }
}

fn parked(on: WaitedOn, value: u64) -> Event {
    Event::Parked { on, value }
}

fn span(
    thread: u32,
    span_id: u64,
    parent: Option<u64>,
    name: &str,
    start_ns: i64,
    dur_ns: i64,
) -> SpanIn {
    SpanIn {
        process: P,
        thread,
        span_id,
        parent,
        depth: 0,
        name: name.to_owned(),
        krate: None,
        function: None,
        unit: None,
        awaits: None,
        owner: None,
        job: None,
        creator: None,
        start_ns,
        dur_ns,
        self_cpu_ns: None,
        folded_ns: 0,
    }
}

fn model(switches: Vec<SwitchIn>, spans: Vec<SpanIn>, build_row: Option<(i64, i64)>) -> Model {
    Model::build(Input { build: 1, build_row, switches, spans, events: Vec::new() }).unwrap()
}

fn job_lane(model: &Model, job: u64) -> &Lane {
    &model.lanes[model.lane(LaneId::Job { process: P, job }).unwrap()]
}

fn states(lane: &Lane) -> Vec<(&'static str, i64, i64)> {
    lane.intervals.iter().map(|it| (it.state.name(), it.from_ns, it.to_ns)).collect()
}

fn span_time<'a>(model: &'a Model, span_id: u64) -> &'a SpanTime {
    &model.span_time[model.spans.iter().position(|it| it.span_id == span_id).unwrap()]
}

#[test]
fn a_job_parked_on_one_thread_and_resumed_on_another_is_one_lane_whose_states_add_up_to_its_life() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 10, parked(WaitedOn::Key, 9), 1, 1),
            switch(2, 30, Event::Resumed, 1, 2),
            switch(2, 50, Event::Ended, 1, 2),
        ],
        vec![],
        None,
    );
    let lane = job_lane(&m, 1);
    assert_eq!(
        states(lane),
        [("running", 0, 10), ("parked_or_queued", 10, 30), ("running", 30, 50)]
    );
    let cores: Vec<_> = lane
        .intervals
        .iter()
        .filter_map(|it| match it.state {
            State::Running { core, .. } => core,
            _ => None,
        })
        .collect();
    assert_eq!(cores, [1, 2], "each run is on the core it ran on");
    assert_eq!(
        lane.totals(),
        Totals { running_ns: 30, parked_or_queued_ns: 20, ..Totals::default() }
    );
    assert_eq!((lane.begin, lane.end), (Begin::Started, End::Ended));
}

#[test]
fn a_ready_row_splits_a_wait_into_parked_then_queued_and_names_its_waker() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(2, 0, Event::Started, 2, 2),
            switch(2, 10, parked(WaitedOn::Key, 3), 2, 2),
            ready(1, 20, 2, Some(1), 1),
            switch(1, 25, Event::Ended, 1, 1),
            switch(1, 30, Event::Resumed, 2, 1),
            switch(1, 40, Event::Ended, 2, 1),
        ],
        vec![],
        None,
    );
    let lane = job_lane(&m, 2);
    assert_eq!(
        states(lane),
        [("running", 0, 10), ("parked", 10, 20), ("queued", 20, 30), ("running", 30, 40)]
    );
    let waker = m.waits[0].waker.lane().unwrap();
    assert_eq!(m.lanes[waker].id, LaneId::Job { process: P, job: 1 });
    assert_eq!(m.waits[0].waker, Waker::Ready { lane: waker });
}

#[test]
fn a_move_to_another_core_is_queued_time_and_no_wait() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 5, parked(WaitedOn::Moved, 0), 1, 1),
            switch(2, 8, Event::Resumed, 1, 2),
            switch(2, 9, Event::Ended, 1, 2),
        ],
        vec![],
        None,
    );
    assert_eq!(states(job_lane(&m, 1)), [("running", 0, 5), ("queued", 5, 8), ("running", 8, 9)]);
}

/// A unit span from 0 to 100 on job 1: a query from 10 to 20, a named wait from 20 to 60 (the
/// job parked 25 to 60 inside it), and a park from 70 to 80 that no wait span names.
#[test]
fn a_units_uncovered_time_is_its_running_under_no_child_and_its_waits_are_not_work() {
    let mut unit = span(1, 1, None, "unit codegen", 0, 100);
    unit.krate = Some("build-script-build".into());
    unit.job = Some(1);
    let query = span(1, 2, Some(1), "infer_query", 10, 10);
    let mut wait = span(1, 3, Some(1), WAIT_SPAN, 20, 40);
    wait.awaits = Some("fact_value(Id(4))".into());
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 25, parked(WaitedOn::Key, 4), 1, 1),
            switch(2, 60, Event::Resumed, 1, 2),
            switch(2, 70, parked(WaitedOn::Counter, 2), 1, 2),
            switch(1, 80, Event::Resumed, 1, 1),
            switch(1, 100, Event::Ended, 1, 1),
        ],
        vec![unit, query, wait],
        None,
    );
    let unit = span_time(&m, 1);
    assert_eq!(
        unit.totals,
        Totals { running_ns: 55, parked_or_queued_ns: 45, ..Totals::default() }
    );
    assert_eq!(unit.totals.total(), 100, "its states add up to its wall time");
    assert_eq!(
        unit.uncovered_ns(),
        55 - 10 - 5,
        "running less the query's 10 and the wait span's 5 before its park"
    );
    assert_eq!(unit.own.waiting_ns(), 10, "the park no wait span names stays the unit's own");
    assert_eq!(
        span_time(&m, 3).totals,
        Totals { running_ns: 5, parked_or_queued_ns: 35, ..Totals::default() }
    );
    assert_eq!(m.wait_label(&m.waits[0]), "fact_value(Id(4))");
    assert_eq!(m.waits[0].span, Some(2));
    assert_eq!(m.waits[1].span, None);

    let rows = report::rows(&m, By::Unit, Some("build-script"));
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].uncovered_ns, rows[0].unnamed_wait_ns), (40, 10));
    let waits = report::rows(&m, By::Wait, None);
    let keys: Vec<&str> = waits.iter().map(|it| it.key.as_str()).collect();
    assert_eq!(
        keys,
        ["key fact_value", "counter wait with no wait span"],
        "a wait with no span is named by its kind, not its raw value"
    );
}

/// Build 53 has no `ended` row the model reads (they carried no build): the old model ran a
/// finished job's last run to its thread's next switch, or its last span's end, a guess
/// that put 15,958 s of 17,053 s on such runs. A job with no `ended` row now has no end.
#[test]
fn a_run_with_no_ended_row_is_unrecorded_to_its_threads_next_switch_and_never_an_end() {
    let m = model(
        vec![
            switch(1, 0, Event::Resumed, 1, 1),
            switch(1, 40, Event::Resumed, 2, 1),
            switch(1, 50, parked(WaitedOn::Counter, 1), 2, 1),
        ],
        vec![span(1, 1, None, "unit link", 5, 20)],
        Some((0, 60)),
    );
    let lane = job_lane(&m, 1);
    assert_eq!(states(lane), [("unrecorded", 0, 40)]);
    assert_eq!(lane.intervals[0].state, State::Unrecorded { why: Unrecorded::NoEnd });
    assert_eq!((lane.begin, lane.end), (Begin::BeforeRecord, End::Unrecorded));
    assert!(lane.spans.is_empty(), "a span naming no job is its thread's, never a guess by time");
    assert_eq!(
        states(job_lane(&m, 2)),
        [("running", 40, 50), ("parked_or_queued", 50, 60)],
        "waiting at the build's end"
    );
}

/// The pool opens a job's `job` span before the job joins the graph: the span names the job
/// by its number then, which the `started` row's `began_as` maps to the job's lane, and the
/// job's run is from its `since_ns`.
#[test]
fn a_job_span_is_laid_on_the_job_it_names_by_its_number_before_it_joined_the_graph() {
    let early = 7 << 32 | crate::flight::UNJOINED_SLOT;
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 7, Event::Ended, 1, 1),
            started(1, 12, 2, 1, 9, early, Some(1)),
            switch(1, 90, Event::Ended, 2, 1),
        ],
        vec![job_span(1, 1, 1, 0, 7), job_span(1, 2, early, 9, 81), job_span(1, 3, 99, 7, 1)],
        Some((0, 90)),
    );
    let job = |span_id| span_time(&m, span_id).lane.and_then(|it| m.lanes[it].id.job());
    assert_eq!((job(1), job(2), job(3)), (Some(1), Some(2), None));
    assert_eq!(span_time(&m, 2).totals, Totals { running_ns: 81, ..Totals::default() });
    assert_eq!(states(job_lane(&m, 2)), [("running", 9, 90)]);
    assert_eq!(job_lane(&m, 2).spawner, m.lane(LaneId::Job { process: P, job: 1 }));
    assert_eq!(m.anomalies.spans_of_no_lane, 1, "job 99 has no switch rows");
}

/// A pool job that never joined the graph has one row, `ran`, at its end.
#[test]
fn a_ran_row_is_one_run_from_its_since_to_its_end() {
    let m = model(
        vec![SwitchIn { since_ns: Some(3), ..switch(1, 10, Event::Ran, 5, 1) }],
        vec![],
        Some((0, 10)),
    );
    let lane = job_lane(&m, 5);
    assert_eq!(states(lane), [("running", 3, 10)]);
    assert_eq!((lane.begin, lane.end), (Begin::Started, End::Ended));
}

#[test]
fn a_run_cut_by_another_jobs_switch_on_its_thread_is_unrecorded_cut() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 10, Event::Started, 2, 1),
            switch(1, 20, Event::Ended, 2, 1),
            switch(1, 30, parked(WaitedOn::Key, 1), 1, 1),
        ],
        vec![],
        Some((0, 30)),
    );
    let lane = job_lane(&m, 1);
    assert_eq!(
        states(lane),
        [("unrecorded", 0, 30)],
        "when the run stopped between 0 and 10 is lost"
    );
    assert_eq!(lane.intervals[0].state, State::Unrecorded { why: Unrecorded::Cut });
    assert_eq!(m.anomalies.runs_cut, 1);
}

#[test]
fn a_waker_is_the_job_a_row_names_or_a_typed_reason_none_is_known() {
    let mut wait = span(2, 3, Some(2), WAIT_SPAN, 10, 10);
    wait.owner = Some(42);
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(2, 0, Event::Started, 2, 2),
            switch(2, 10, parked(WaitedOn::Key, 3), 2, 2),
            switch(2, 20, Event::Resumed, 2, 2),
            switch(2, 30, parked(WaitedOn::Counter, 3), 2, 2),
            ready(1, 35, 2, None, 1),
            switch(2, 40, Event::Resumed, 2, 2),
            switch(2, 50, parked(WaitedOn::Job, 1), 2, 2),
            switch(1, 60, Event::Ended, 1, 1),
            switch(2, 60, Event::Resumed, 2, 2),
            switch(2, 70, Event::Ended, 2, 2),
        ],
        vec![job_span(2, 2, 2, 0, 70), wait],
        Some((0, 70)),
    );
    let one = m.lane(LaneId::Job { process: P, job: 1 }).unwrap();
    let wakers: Vec<_> = m.waits.iter().map(|it| it.waker).collect();
    assert_eq!(wakers, [Waker::NoLane { job: 42 }, Waker::NoJob, Waker::Awaited { lane: one }]);
}

#[test]
fn the_critical_path_crosses_from_a_key_wait_to_the_job_running_the_key() {
    let mut unit = span(11, 2, None, "unit b", 10, 90);
    unit.krate = Some("b".into());
    unit.job = Some(1 << 32 | 2);
    let mut wait = span(11, 3, Some(2), WAIT_SPAN, 20, 40);
    wait.awaits = Some("key a".into());
    wait.owner = Some(1 << 32 | 1);
    let mut compile = span(10, 1, None, "compile a", 0, 60);
    compile.krate = Some("a".into());
    compile.job = Some(1 << 32 | 1);
    let m = model(
        vec![
            switch(10, 0, Event::Started, 1 << 32 | 1, 1),
            switch(11, 10, Event::Started, 1 << 32 | 2, 2),
            switch(11, 21, parked(WaitedOn::Key, 5), 1 << 32 | 2, 2),
            switch(10, 60, Event::Ended, 1 << 32 | 1, 1),
            switch(11, 60, Event::Resumed, 1 << 32 | 2, 2),
            switch(11, 100, Event::Ended, 1 << 32 | 2, 2),
        ],
        vec![compile, unit, wait],
        Some((0, 100)),
    );
    let owner = m.lane(LaneId::Job { process: P, job: 1 << 32 | 1 }).unwrap();
    assert_eq!(
        m.waits[0].waker,
        Waker::Owner { lane: owner },
        "the owner column names the runner's job"
    );
    let path = path::critical(&m);
    let links: Vec<_> = path
        .links
        .iter()
        .map(|it| (it.kind, m.lanes[it.lane].id.job(), it.from_ns, it.to_ns))
        .collect();
    assert_eq!(
        links,
        [(LinkKind::Ran, Some(1 << 32 | 1), 0, 60), (LinkKind::Ran, Some(1 << 32 | 2), 60, 100)],
        "the wait is replaced by its waker's run"
    );
    assert_eq!(path.reached, Reached::BuildStart);
    assert_eq!(path.links[0].wakes, Some(0));
    assert_eq!(report::link_item(&m, &path.links[0]), "compile a a");
}

/// Job 2 is given to the pool by job 1 at 5 and starts at 30, after job 1 ended at 10: its
/// path runs back through its queueing to its spawner's run, to the build's start.
#[test]
fn the_critical_path_crosses_from_a_jobs_start_to_its_spawner() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 10, Event::Ended, 1, 1),
            SwitchIn { by: Some(1), ..switch(2, 30, Event::Started, 2, 2) },
            switch(2, 50, Event::Ended, 2, 2),
        ],
        vec![],
        Some((0, 50)),
    );
    let path = path::critical(&m);
    let links: Vec<_> = path
        .links
        .iter()
        .map(|it| (it.kind, m.lanes[it.lane].id.job(), it.from_ns, it.to_ns))
        .collect();
    assert_eq!(
        links,
        [
            (LinkKind::Ran, Some(1), 0, 10),
            (LinkKind::Spawned, Some(2), 10, 30),
            (LinkKind::Ran, Some(2), 30, 50)
        ]
    );
    assert_eq!(path.reached, Reached::BuildStart);
    let alone = model(
        vec![switch(2, 30, Event::Started, 2, 2), switch(2, 50, Event::Ended, 2, 2)],
        vec![],
        Some((0, 50)),
    );
    assert!(
        matches!(path::critical(&alone).reached, Reached::LaneStart { .. }),
        "no row names its spawner"
    );
}

/// Job 2 is given to the pool at 15 by a thread that runs no job (a build's driver, the
/// daemon's main loop), inside its span 1, which ends at 20; the job starts at 30. No row names
/// a spawning job, so the path crosses through the creator span its `job` span names: back
/// over the giver's running to the build's start, its links adding up to the wall.
#[test]
fn the_critical_path_crosses_from_a_job_to_the_span_that_gave_it_on_a_thread_with_no_job() {
    let rows = |creator: Option<u64>| {
        let mut job = job_span(2, 7, 2, 30, 20);
        job.creator = creator;
        model(
            vec![switch(2, 30, Event::Started, 2, 2), switch(2, 50, Event::Ended, 2, 2)],
            vec![span(5, 1, None, "build driver", 0, 20), job],
            Some((0, 50)),
        )
    };
    let m = rows(Some(1));
    let path = path::critical(&m);
    let driver = m.lane(LaneId::Thread { process: P, thread: 5 }).unwrap();
    let links: Vec<_> =
        path.links.iter().map(|it| (it.kind, it.lane, it.from_ns, it.to_ns)).collect();
    let job = m.lane(LaneId::Job { process: P, job: 2 }).unwrap();
    assert_eq!(
        links,
        [
            (LinkKind::Ran, driver, 0, 20),
            (LinkKind::Spawned, job, 20, 30),
            (LinkKind::Ran, job, 30, 50)
        ]
    );
    assert_eq!(path.reached, Reached::BuildStart);
    assert_eq!(path.len_ns(), 50, "the path is the wall");
    assert_eq!(m.anomalies.creators_unread, 0);

    let unnamed = rows(None);
    assert!(
        matches!(path::critical(&unnamed).reached, Reached::LaneStart { .. }),
        "no row names who gave it"
    );
    let unread = rows(Some(99));
    assert!(matches!(path::critical(&unread).reached, Reached::LaneStart { .. }));
    assert_eq!(
        unread.anomalies.creators_unread, 1,
        "a creator with no row is counted, not guessed"
    );
}

#[test]
fn a_wait_with_no_waker_stays_on_the_path_as_a_wait() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 10, parked(WaitedOn::Counter, 3), 1, 1),
            switch(1, 30, Event::Resumed, 1, 1),
            switch(1, 40, Event::Ended, 1, 1),
        ],
        vec![],
        Some((0, 40)),
    );
    let path = path::critical(&m);
    let kinds: Vec<_> = path.links.iter().map(|it| it.kind).collect();
    assert_eq!(kinds, [LinkKind::Ran, LinkKind::Waited, LinkKind::Ran]);
    assert_eq!(path.len_ns(), 40);
}

#[test]
fn a_thread_no_switch_names_is_a_lane_running_between_its_wait_spans() {
    let outer = span(5, 1, None, "client", 0, 50);
    let wait = span(5, 2, Some(1), WAIT_SPAN, 10, 30);
    let later = span(5, 3, None, "client again", 80, 20);
    let m = model(vec![], vec![outer, wait, later], None);
    let lane = &m.lanes[m.lane(LaneId::Thread { process: P, thread: 5 }).unwrap()];
    assert_eq!(
        states(lane),
        [
            ("running", 0, 10),
            ("parked_or_queued", 10, 40),
            ("running", 40, 50),
            ("unrecorded", 50, 80),
            ("running", 80, 100)
        ]
    );
    assert_eq!(span_time(&m, 1).uncovered_ns(), 20);
}

#[test]
fn a_spans_folded_children_are_covered_not_its_own() {
    let outer = SpanIn { folded_ns: 6, ..span(5, 1, None, "client", 0, 50) };
    let wait = span(5, 2, Some(1), WAIT_SPAN, 10, 30);
    let m = model(vec![], vec![outer, wait], None);
    assert_eq!(
        span_time(&m, 1).uncovered_ns(),
        20 - 6,
        "running 20 under no row, 6 of it under folded children"
    );
}

#[test]
fn the_cores_busy_time_is_each_runs_overlap_with_each_bucket() {
    let m = model(
        vec![
            switch(1, 0, Event::Started, 1, 1),
            switch(1, 15, Event::Ended, 1, 1),
            switch(2, 5, Event::Started, 2, 2),
            switch(2, 20, Event::Ended, 2, 2),
        ],
        vec![],
        Some((0, 20)),
    );
    let busy: Vec<_> =
        report::core_busy(&m, 10).iter().map(|it| (it.core, it.t0_ns, it.running_ns)).collect();
    assert_eq!(busy, [(1, 0, 10), (1, 10, 5), (2, 0, 5), (2, 10, 10)]);
    let util = report::utilization(&m);
    assert_eq!((util.cores, util.running_ns, util.wall_ns), (2, 30, 20));
}

#[test]
fn a_switch_names_only_the_recorders_events() {
    for it in Switched::ALL {
        let wait = (it == Switched::Parked).then_some(("key", Some(3)));
        assert!(Event::of(it.name(), wait).is_ok(), "{}", it.name());
    }
    assert!(matches!(Event::of("parked", None), Err(TimeError::BadSwitch { .. })));
    assert!(matches!(Event::of("parked", Some(("key", None))), Err(TimeError::BadSwitch { .. })));
    assert_eq!(Event::of("parked", Some(("moved", None))).unwrap(), parked(WaitedOn::Moved, 0));
    assert!(matches!(Event::of("napped", None), Err(TimeError::BadSwitch { .. })));
}

#[test]
fn a_build_with_no_rows_is_an_error_naming_it() {
    let error = Model::build(Input { build: 4, ..Input::default() }).unwrap_err();
    assert!(matches!(error, TimeError::Empty { build: 4 }), "{error}");
}

#[test]
fn the_trace_has_a_track_per_fiber_and_a_flow_from_waker_to_woken() {
    let mut wait = span(11, 3, Some(4), WAIT_SPAN, 20, 40);
    wait.owner = Some(1 << 32 | 1);
    let root = job_span(11, 4, 1 << 32 | 2, 10, 90);
    let m = model(
        vec![
            switch(10, 0, Event::Started, 1 << 32 | 1, 1),
            switch(11, 10, Event::Started, 1 << 32 | 2, 2),
            switch(11, 21, parked(WaitedOn::Key, 5), 1 << 32 | 2, 2),
            switch(10, 60, Event::Ended, 1 << 32 | 1, 1),
            switch(11, 60, Event::Resumed, 1 << 32 | 2, 2),
            switch(11, 100, Event::Ended, 1 << 32 | 2, 2),
        ],
        vec![root, wait],
        Some((0, 100)),
    );
    let events = trace::events(&m, |ns| ns as f64 / 1e3);
    let of = |ph: &str| events.iter().filter(|it| it["ph"] == ph).count();
    assert_eq!((of("s"), of("f")), (1, 1), "one wake, one arrow");
    let parks: Vec<_> = events.iter().filter(|it| it["cat"] == "parked_or_queued").collect();
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0]["tid"], trace::FIBER_TRACK | 2);
    let html = html::page(&m, &path::critical(&m), 10);
    assert!(html.starts_with("<!doctype html>") && !html.contains("http"), "self-contained");
}

// --- Properties over generated schedules ------------------------------------------------

/// A schedule a pool of `threads` workers could run: jobs start, park (on a key, a counter, or
/// to move), become ready, resume on any idle thread (so a job migrates), and end, while
/// opening and closing nested spans, a wait span around each key park.
fn schedule(tc: &TestCase) -> (Vec<SwitchIn>, Vec<SpanIn>, i64) {
    #[derive(Clone)]
    enum Job {
        New,
        Running { thread: u32 },
        Parked { wait_span: Option<usize>, ready: bool },
        Ended,
    }
    let threads = tc.draw(gs::integers::<u32>().min_value(1).max_value(4));
    let jobs = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let steps = tc.draw(gs::integers::<usize>().max_value(80));
    let mut state = vec![Job::New; jobs];
    let mut busy: Vec<Option<usize>> = vec![None; threads as usize];
    // Each job's open spans, innermost last, as indexes into `spans`.
    let mut open: Vec<Vec<usize>> = vec![Vec::new(); jobs];
    let (mut switches, mut spans): (Vec<SwitchIn>, Vec<(SpanIn, bool)>) = (Vec::new(), Vec::new());
    let mut t = 0i64;
    let id = |job: usize| (job as u64 + 1) | 1 << 32;
    for _ in 0..steps {
        // At least 1 ns apart: two rows of one job at one instant are ordered by rank alone.
        t += tc.draw(gs::integers::<i64>().min_value(1).max_value(50));
        let job = tc.draw(gs::integers::<usize>().max_value(jobs - 1));
        let idle = busy.iter().position(Option::is_none);
        let core_of = |thread: u32| thread + 100;
        let ready_now =
            matches!(state[job], Job::Parked { ready: false, .. }) && tc.draw(gs::booleans());
        match state[job].clone() {
            Job::Parked { wait_span, ready: false } if ready_now => {
                let by = (0..jobs).find(|it| matches!(state[*it], Job::Running { .. }));
                let thread = by.and_then(|it| match state[it] {
                    Job::Running { thread } => Some(thread),
                    _ => None,
                });
                let Some(thread) = thread else { continue };
                switches.push(SwitchIn {
                    by: by.map(id),
                    ..SwitchIn::of(
                        P,
                        thread + 1,
                        t,
                        Event::Ready,
                        id(job),
                        job as u32,
                        core_of(thread),
                    )
                });
                state[job] = Job::Parked { wait_span, ready: true };
            }
            Job::New | Job::Parked { .. } if idle.is_some() => {
                let thread = tc.draw(gs::integers::<u32>().max_value(threads - 1));
                if busy[thread as usize].is_some() {
                    continue;
                }
                let event = match state[job] {
                    Job::New => Event::Started,
                    _ => Event::Resumed,
                };
                if let Job::Parked { wait_span: Some(at), .. } = state[job] {
                    spans[at].0.dur_ns = t - spans[at].0.start_ns;
                    spans[at].1 = true;
                    open[job].retain(|it| *it != at);
                }
                switches.push(SwitchIn::of(
                    P,
                    thread + 1,
                    t,
                    event,
                    id(job),
                    job as u32,
                    core_of(thread),
                ));
                busy[thread as usize] = Some(job);
                state[job] = Job::Running { thread };
            }
            Job::Running { thread } => {
                let row = |event| {
                    SwitchIn::of(P, thread + 1, t, event, id(job), job as u32, core_of(thread))
                };
                match tc.draw(gs::integers::<u8>().max_value(4)) {
                    0 => {
                        let parent = open[job].last().map(|it| spans[*it].0.span_id);
                        let mut it = span(thread + 1, spans.len() as u64 + 1, parent, "work", t, 0);
                        it.depth = open[job].len() as u16;
                        // The record names the job on its root span; on every span here, so
                        // one whose parent never closed is still laid by its row.
                        it.job = Some(id(job));
                        open[job].push(spans.len());
                        spans.push((it, false));
                    }
                    1 => {
                        if let Some(at) = open[job].pop() {
                            spans[at].0.dur_ns = t - spans[at].0.start_ns;
                            spans[at].1 = true;
                        }
                    }
                    2 => {
                        let on = match tc.draw(gs::integers::<u8>().max_value(2)) {
                            0 => WaitedOn::Key,
                            1 => WaitedOn::Counter,
                            _ => WaitedOn::Moved,
                        };
                        let wait_span = (on == WaitedOn::Key).then(|| {
                            let parent = open[job].last().map(|it| spans[*it].0.span_id);
                            let mut it =
                                span(thread + 1, spans.len() as u64 + 1, parent, WAIT_SPAN, t, 0);
                            it.depth = open[job].len() as u16;
                            it.job = Some(id(job));
                            // The owner column is the runner's job, as `switches.job` names it.
                            it.owner =
                                Some(id(tc.draw(gs::integers::<usize>().max_value(jobs - 1))));
                            open[job].push(spans.len());
                            spans.push((it, false));
                            spans.len() - 1
                        });
                        // A wait span opens on the running fiber, before its park.
                        let mut park = row(Event::Parked { on, value: 1 });
                        park.t_ns += 1;
                        t += 1;
                        switches.push(park);
                        busy[thread as usize] = None;
                        state[job] = Job::Parked { wait_span, ready: false };
                    }
                    3 if open[job].is_empty() => {
                        switches.push(row(Event::Ended));
                        busy[thread as usize] = None;
                        state[job] = Job::Ended;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    t += 1;
    for (job, it) in state.iter().enumerate() {
        if let Job::Running { .. } = it {
            for &at in open[job].iter().rev() {
                spans[at].0.dur_ns = t - spans[at].0.start_ns;
                spans[at].1 = true;
            }
        }
    }
    // A span is a row only once it closed.
    let spans = spans.into_iter().filter(|it| it.1).map(|it| it.0).collect();
    (switches, spans, t)
}

fn generated(tc: &TestCase) -> Model {
    let (switches, spans, end) = schedule(tc);
    let empty = switches.is_empty() && spans.is_empty();
    match Model::build(Input {
        build: 1,
        build_row: Some((0, end)),
        switches,
        spans,
        events: Vec::new(),
    }) {
        Ok(it) => it,
        Err(TimeError::Empty { .. }) if empty => {
            model(vec![switch(1, 0, Event::Started, 1, 1)], vec![], Some((0, 1)))
        }
        Err(error) => panic!("{error}"),
    }
}

#[hegel::test(test_cases = 300)]
fn every_lane_is_cut_end_to_end_and_its_states_add_up_to_its_lifetime(tc: TestCase) {
    let m = generated(&tc);
    for lane in &m.lanes {
        for pair in lane.intervals.windows(2) {
            assert_eq!(pair[0].to_ns, pair[1].from_ns, "{}: intervals meet", lane.id);
        }
        assert!(lane.intervals.iter().all(|it| it.dur_ns() > 0), "{}: no empty interval", lane.id);
        assert_eq!(lane.totals().total(), lane.end_ns() - lane.begin_ns(), "{}", lane.id);
    }
}

#[hegel::test(test_cases = 300)]
fn every_spans_states_add_up_to_its_wall_time_and_its_own_time_is_never_negative(tc: TestCase) {
    let m = generated(&tc);
    assert_eq!(m.anomalies.misnested, 0, "the schedule nests its spans");
    for (span, time) in m.spans.iter().zip(&m.span_time) {
        assert_eq!(time.totals.total(), span.dur_ns, "span {}", span.span_id);
        let own = time.own;
        for (what, ns) in [
            ("running", own.running_ns),
            ("parked", own.parked_ns),
            ("queued", own.queued_ns),
            ("parked or queued", own.parked_or_queued_ns),
            ("unrecorded", own.unrecorded_ns),
        ] {
            assert!(ns >= 0, "span {}: own {what} {ns}", span.span_id);
        }
    }
    // Uncovered time is never hidden: a lane's running is its spans' own running plus what ran
    // under no span, so the spans' own running never exceeds it.
    for (l, lane) in m.lanes.iter().enumerate() {
        let own: i64 = lane.spans.iter().map(|&i| m.span_time[i].uncovered_ns()).sum();
        assert!(own <= lane.totals().running_ns, "lane {l}");
        assert!(lane.spans.iter().all(|&i| m.span_time[i].lane == Some(l)));
    }
}

#[hegel::test(test_cases = 300)]
fn no_two_runs_share_a_core_so_running_is_at_most_cores_times_wall(tc: TestCase) {
    let m = generated(&tc);
    let mut runs: Vec<(u32, i64, i64)> = Vec::new();
    for lane in &m.lanes {
        for it in &lane.intervals {
            if let State::Running { core: Some(core), .. } = it.state {
                runs.push((core, it.from_ns, it.to_ns));
            }
        }
    }
    runs.sort_unstable();
    for pair in runs.windows(2) {
        if pair[0].0 == pair[1].0 {
            assert!(pair[0].2 <= pair[1].1, "two runs on core {} overlap: {pair:?}", pair[0].0);
        }
    }
    let util = report::utilization(&m);
    assert!(util.running_ns <= i64::from(util.cores) * util.wall_ns);
    for it in report::core_busy(&m, 7) {
        assert!(it.running_ns <= it.t1_ns - it.t0_ns, "{it:?}");
    }
}

#[hegel::test(test_cases = 300)]
fn the_critical_path_is_contiguous_inside_the_build_and_no_longer_than_it(tc: TestCase) {
    let m = generated(&tc);
    let path = path::critical(&m);
    assert!(!matches!(path.reached, Reached::Fuel { .. }), "the walk ends within its bound");
    for pair in path.links.windows(2) {
        assert_eq!(pair[0].to_ns, pair[1].from_ns, "links meet");
    }
    assert!(path.links.iter().all(|it| it.dur_ns() > 0));
    if let (Some(start), Some(end)) = (path.start_ns(), path.end_ns()) {
        assert!(start >= m.bounds.start_ns && end <= m.bounds.end_ns);
        assert_eq!(path.len_ns(), end - start);
    }
}

/// `it` in an order `seed` picks.
fn shuffled<T>(mut it: Vec<T>, seed: u64) -> Vec<T> {
    blaze_ids::rng::SplitMix64::new(seed).shuffle(&mut it);
    it
}

#[hegel::test(test_cases = 200)]
fn the_model_does_not_depend_on_the_order_rows_came_in(tc: TestCase) {
    let (switches, spans, end) = schedule(&tc);
    if switches.is_empty() && spans.is_empty() {
        return;
    }
    let seed = tc.draw(gs::integers::<u64>());
    let a = Model::build(Input {
        build: 1,
        build_row: Some((0, end)),
        switches: switches.clone(),
        spans: spans.clone(),
        events: Vec::new(),
    })
    .unwrap();
    let b = Model::build(Input {
        build: 1,
        build_row: Some((0, end)),
        switches: shuffled(switches, seed),
        spans: shuffled(spans, seed),
        events: Vec::new(),
    })
    .unwrap();
    let lanes = |m: &Model| m.lanes.iter().map(|it| (it.id, states(it))).collect::<Vec<_>>();
    assert_eq!(lanes(&a), lanes(&b));
    let by_id = |m: &Model| {
        let mut it: Vec<_> =
            m.spans.iter().zip(&m.span_time).map(|(s, t)| (s.span_id, t.totals, t.own)).collect();
        it.sort_by_key(|it| it.0);
        it
    };
    assert_eq!(by_id(&a), by_id(&b));
    assert_eq!(report::rows(&a, By::Wait, None), report::rows(&b, By::Wait, None));
}

fn event(thread: u32, t_ns: i64, job: Option<u64>, span: Option<u64>, message: &str) -> EventIn {
    EventIn {
        process: P,
        thread,
        t_ns,
        build: None,
        job,
        level: "WARN".to_owned(),
        target: "blaze_cargo::serve::daemon".to_owned(),
        name: "event daemon.rs:1".to_owned(),
        message: Some(message.to_owned()),
        span,
        event_id: t_ns as u64,
        fields: Vec::new(),
    }
}

#[test]
fn an_event_is_on_its_jobs_lane_else_its_spans_else_its_threads_and_the_trace_and_page_show_it() {
    let input = Input {
        build: 1,
        build_row: None,
        switches: vec![switch(1, 0, Event::Started, 1, 1), switch(1, 50, Event::Ended, 1, 1)],
        // Thread 2 runs no job: its span makes it a thread lane.
        spans: vec![span(2, 20, None, "unit", 0, 40)],
        events: vec![
            event(1, 30, Some(1), None, "of the job"),
            event(2, 10, None, Some(20), "in the span"),
            event(2, 45, None, None, "on the thread"),
            event(3, 5, None, None, "on no lane"),
        ],
    };
    let m = Model::build(input).unwrap();
    let job = m.lane(LaneId::Job { process: P, job: 1 });
    let thread = m.lane(LaneId::Thread { process: P, thread: 2 });
    let lane_of = |message: &str| {
        m.event_lane[m.events.iter().position(|it| it.message.as_deref() == Some(message)).unwrap()]
    };
    assert!(job.is_some() && thread.is_some());
    assert_eq!(lane_of("of the job"), job);
    assert_eq!(lane_of("in the span"), thread);
    assert_eq!(lane_of("on the thread"), thread);
    assert_eq!(lane_of("on no lane"), None);
    let times: Vec<_> = m.events.iter().map(|it| it.t_ns).collect();
    assert_eq!(times, [5, 10, 30, 45], "events are in time order");

    let trace = super::trace::events(&m, |ns| ns as f64 / 1e3);
    let of_job = trace
        .iter()
        .find(|it| it["name"] == "WARN of the job")
        .expect("the job's event is an instant");
    assert_eq!(
        (of_job["ph"].as_str(), of_job["tid"].as_u64()),
        (Some("i"), Some(super::trace::FIBER_TRACK | 1))
    );
    let lost = trace
        .iter()
        .find(|it| it["name"] == "WARN on no lane")
        .expect("a lane-less event is an instant");
    assert_eq!(lost["tid"].as_u64(), Some(3), "on its thread's own track");

    let page = super::html::page(&m, &path::critical(&m), 10);
    assert!(page.contains("<h2>Events</h2>") && page.contains("of the job"), "{page}");
}
