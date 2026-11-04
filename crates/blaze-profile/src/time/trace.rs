//! The model as Chrome trace events (ui.perfetto.dev): a track per fiber with its jobs' runs,
//! parks and queueing as slices, a track per core with the runs on it, a flow arrow from
//! each waker's slice to the run its wake resumed, and each kept `tracing` event as an instant
//! on its job's track. The flight dump (crate::dump::chrome) puts these
//! beside the threads' spans; `blaze debug time --trace` writes them alone.

use serde_json::{Value, json};

use super::{LaneId, Model, State, report};

/// The track id of a fiber's lane, beside a thread's own (its id), its steps (1 << 32) and
/// its samples (2 << 32) in crate::dump::chrome.
pub const FIBER_TRACK: u64 = 3 << 32;
/// The track id of a core.
pub const CORE_TRACK: u64 = 4 << 32;
/// The track id of a thread lane: a thread no switch puts a job on.
pub const THREAD_LANE_TRACK: u64 = 5 << 32;

fn track(model: &Model, lane: usize) -> u64 {
    let lane = &model.lanes[lane];
    match (lane.id, lane.fiber) {
        (LaneId::Job { .. }, Some(fiber)) => FIBER_TRACK | u64::from(fiber),
        (LaneId::Job { job, .. }, None) => FIBER_TRACK | (1 << 31) | (job & 0x7fff_ffff),
        (LaneId::Thread { thread, .. }, _) => THREAD_LANE_TRACK | u64::from(thread),
    }
}

/// The events of `model`, each instant made a trace's microseconds by `us`.
pub fn events(model: &Model, us: impl Fn(i64) -> f64) -> Vec<Value> {
    // alloc: a trace's events, once a trace
    let mut out = Vec::new();
    // alloc: once a trace
    let mut named = blaze_hash::HashSet::new_in(std::alloc::Global);
    let mut name_track = |out: &mut Vec<Value>, pid: u32, tid: u64, name: String| {
        if named.insert((pid, tid)) {
            out.push(json!({"ph": "M", "name": "thread_name", "pid": pid, "tid": tid, "args": {"name": name}}));
            out.push(json!({"ph": "M", "name": "thread_sort_index", "pid": pid, "tid": tid, "args": {"sort_index": tid}}));
        }
    };
    for (l, lane) in model.lanes.iter().enumerate() {
        let pid = lane.id.process();
        let tid = track(model, l);
        let title = match lane.fiber {
            // alloc: a track's name
            Some(fiber) => format!("fiber {fiber}"),
            // alloc: a track's name
            None => format!("{} (no fiber)", lane.id),
        };
        name_track(&mut out, pid, tid, title);
        for it in &lane.intervals {
            let inside = it.inside.map(|span| model.spans[span].item());
            let (name, mut args) = match it.state {
                State::Running { core, thread, end } => (
                    // alloc: a slice's name
                    inside.clone().unwrap_or_else(|| "running in no span".to_owned()),
                    json!({"core": core, "thread": thread, "run_end": end.name()}),
                ),
                State::Parked { wait }
                | State::Queued { wait }
                | State::ParkedOrQueued { wait } => {
                    let it_wait = &model.waits[wait];
                    let label = model.wait_label(it_wait);
                    let waker = it_wait.waker.lane().map(|it| model.lanes[it].id.to_string());
                    (
                        // alloc: a slice's name
                        format!("{}: {label}", it.state.name()),
                        json!({"wait": label, "waker": waker, "waker_known_by": it_wait.waker.name(), "inside": inside}),
                    )
                }
                // alloc: a slice's name
                State::Unrecorded { why } => ("unrecorded".to_owned(), json!({"why": why.name()})),
            };
            args["job"] = json!(lane.id.to_string());
            out.push(
                json!({"ph": "X", "name": name, "cat": it.state.name(), "pid": pid, "tid": tid,
                "ts": us(it.from_ns), "dur": us(it.to_ns) - us(it.from_ns), "args": args}),
            );
            if let State::Running { core: Some(core), .. } = it.state {
                let core_tid = CORE_TRACK | u64::from(core);
                // alloc: a track's name
                name_track(&mut out, pid, core_tid, format!("core {core}"));
                out.push(json!({"ph": "X", "name": format!("{}: {}", lane.id, inside.as_deref().unwrap_or("no span")),
                    "cat": "core", "pid": pid, "tid": core_tid, "ts": us(it.from_ns),
                    "dur": us(it.to_ns) - us(it.from_ns)}));
            }
        }
    }
    for (w, wait) in model.waits.iter().enumerate() {
        let (Some(waker), Some(resumed)) = (wait.waker.lane(), wait.resumed_ns) else { continue };
        let woke = wait.ready_ns.unwrap_or(resumed);
        let lane = &model.lanes[waker];
        // The flow starts inside the waker's slice nearest before the wake, so Perfetto binds it there.
        let at = lane.at_or_before(woke).map(|i| &lane.intervals[i]).or(lane.intervals.last());
        let Some(slice) = at else { continue };
        let from = woke.min(slice.to_ns).max(slice.from_ns);
        let pid = lane.id.process();
        out.push(json!({"ph": "s", "id": w, "name": "wake", "cat": "wake", "pid": pid, "tid": track(model, waker),
            "ts": (us(from) - 0.001).max(us(slice.from_ns))}));
        out.push(json!({"ph": "f", "bp": "e", "id": w, "name": "wake", "cat": "wake", "pid": model.lanes[wait.lane].id.process(),
            "tid": track(model, wait.lane), "ts": us(resumed)}));
    }
    for (it, lane) in model.events.iter().zip(&model.event_lane) {
        // An event on no lane is on its thread's own track, beside the thread's spans.
        let tid = lane.map_or(u64::from(it.thread), |lane| track(model, lane));
        out.push(instant(it, tid, us(it.t_ns)));
    }
    out
}

/// An event as an instant on the track `tid`: `WARN no unit's step ended for 60 s unit=serde`,
/// its fields typed in its args beside what correlates it (build, job, span, its id).
pub(crate) fn instant(it: &super::EventIn, tid: u64, ts: f64) -> Value {
    // alloc: an event's fields, once an event a trace
    let fields: serde_json::Map<String, Value> = it
        .fields
        .iter()
        // clone: a field's key, once an event a trace
        .map(|(key, value)| (key.clone(), value.json()))
        .collect();
    // alloc: an instant's name, once an event a trace
    json!({"ph": "i", "s": "t", "name": format!("{} {}", it.level, it.said()), "cat": "event", "pid": it.process, "tid": tid, "ts": ts,
        "args": {"level": it.level, "target": it.target, "callsite": it.name, "message": it.message, "fields": fields,
            "build": it.build, "job": it.job, "span": it.span, "event_id": it.event_id}})
}

/// The fiber tracks of a flight read, for `trace.json` beside its threads' spans, with its
/// events on them: when the read has no switch, only its events, each on its thread's track;
/// a model the read can't make is an instant event saying why.
pub(crate) fn of_read(
    read: &crate::flight::Read,
    process: u32,
    us: impl Fn(i64) -> f64,
) -> Vec<Value> {
    if read.switches.is_empty() {
        return match super::Input::of_read(read, process) {
            Ok(input) => input
                .events
                .iter()
                .map(|it| instant(it, u64::from(it.thread), us(it.t_ns)))
                .collect(),
            Err(error) => {
                let at = read.events.iter().map(|it| it.t_ns).min().map_or(0.0, &us);
                // alloc: a dump's one event, on demand
                vec![
                    json!({"ph": "i", "s": "p", "name": format!("no events: {error}"), "pid": process, "tid": 0, "ts": at}),
                ]
            }
        };
    }
    match super::Input::of_read(read, process).and_then(Model::build) {
        Ok(model) => events(&model, us),
        Err(error) => {
            let at = read.switches.iter().map(|it| it.t_ns).min().map_or(0.0, &us);
            // alloc: a dump's one event, on demand
            vec![
                json!({"ph": "i", "s": "p", "name": format!("no fiber tracks: {error}"), "pid": process, "tid": 0, "ts": at}),
            ]
        }
    }
}

/// A trace of `model` alone: its events and the critical path's links on a track of their own,
/// microseconds from the build's start.
pub fn json(model: &Model, path: &super::path::Path) -> Vec<u8> {
    let start = model.bounds.start_ns;
    let us = |ns: i64| (ns - start) as f64 / 1e3;
    let mut events = events(model, us);
    let pid = model.lanes.first().map_or(0, |it| it.id.process());
    let tid = CORE_TRACK - 1;
    events.push(json!({"ph": "M", "name": "thread_name", "pid": pid, "tid": tid, "args": {"name": "critical path"}}));
    for link in &path.links {
        events.push(json!({"ph": "X", "name": report::link_item(model, link), "cat": link.kind.name(), "pid": pid,
            "tid": tid, "ts": us(link.from_ns), "dur": us(link.to_ns) - us(link.from_ns),
            "args": {"lane": model.lanes[link.lane].id.to_string()}}));
    }
    // alloc: a trace, once a report
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"traceEvents\":[\n");
    for (i, it) in events.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b",\n");
        }
        out.extend_from_slice(it.to_string().as_bytes());
    }
    out.extend_from_slice(b"\n],\"displayTimeUnit\":\"ms\"}");
    out
}
