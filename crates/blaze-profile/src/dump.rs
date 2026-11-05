//! The flight recorder's dump as a Chrome trace (ui.perfetto.dev): what [`crate::flight::dump`]
//! writes beside a stall's error, of every ring's last window and the CPU samples.
//!
//! This was the writer of `BLAZE_PROFILE`'s cross-process profile until 2026-11-04; only the
//! one-process dump is left, and `BLAZE_PROFILE` is an export from the history now
//! ([`crate::time::load::trace_of`]).

use std::alloc::Global;

use blaze_hash::HashMap;

/// What one process's rings hold, as the dump writes it.
#[derive(Default)]
pub(crate) struct Part {
    pub(crate) pid: u32,
    pub(crate) command: String,
    pub(crate) threads: Vec<(u64, String)>,
    pub(crate) events: Vec<Event>,
    pub(crate) counters: Vec<Counter>,
    pub(crate) samples: Vec<Sampled>,
    /// What the CPU sampler was: on and how, or why it was off.
    pub(crate) sampler: String,
    /// The fibers' tracks as trace events ([`crate::time::trace::of_read`]): their runs on
    /// cores, their parks and the wakes between them.
    pub(crate) fibers: Vec<serde_json::Value>,
}

/// A span: its name, what it is of, its thread and its time, in microseconds.
#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub(crate) name: String,
    pub(crate) subject: Option<String>,
    pub(crate) tid: u64,
    pub(crate) start_us: u64,
    pub(crate) dur_us: u64,
    pub(crate) cpu_us: Option<u64>,
}

/// A [`crate::Plot`]'s value at a moment, on the thread that gave it.
#[derive(Clone, Debug)]
pub(crate) struct Counter {
    pub(crate) name: String,
    pub(crate) tid: u64,
    pub(crate) at_us: u64,
    pub(crate) value: u64,
}

/// A CPU sample: the thread, the moment, and the functions on its stack, innermost first; the
/// span, job and fiber it was taken in (0: none).
#[derive(Clone, Debug)]
pub(crate) struct Sampled {
    pub(crate) tid: u64,
    pub(crate) at_us: u64,
    pub(crate) stack: Vec<String>,
    pub(crate) span: u64,
    pub(crate) job: u64,
    pub(crate) fiber: u32,
}

/// The Chrome trace event format: complete events, with names for the processes and threads.
/// Key `blaze` has the window's start and length, the samples folded and what the sampler was.
pub(crate) fn chrome(
    out: &mut dyn std::io::Write,
    parts: &[Part],
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    use serde_json::json;
    let mut first = true;
    let mut put = |out: &mut dyn std::io::Write, event: serde_json::Value| {
        let comma = if std::mem::take(&mut first) { "" } else { ",\n" };
        write!(out, "{comma}{event}")
    };
    out.write_all(b"{\"traceEvents\":[\n")?;
    let mut named = blaze_hash::HashSet::new_in(Global);
    for part in parts {
        if named.insert((part.pid, u64::MAX)) {
            put(
                &mut *out,
                json!({"ph": "M", "name": "process_name", "pid": part.pid, "tid": 0,
                // alloc: a dump, on demand
                "args": {"name": format!("{} ({})", part.command, part.pid)}}),
            )?;
        }
        for (tid, thread) in &part.threads {
            if named.insert((part.pid, *tid)) {
                put(
                    &mut *out,
                    json!({"ph": "M", "name": "thread_name", "pid": part.pid, "tid": tid,
                    "args": {"name": thread}}),
                )?;
            }
        }
        for it in &part.events {
            let mut args = serde_json::Map::new();
            if let Some(cpu) = it.cpu_us {
                args.insert("cpu_us".into(), cpu.into());
            }
            let name = match &it.subject {
                Some(of) => {
                    // clone: a dump, on demand
                    args.insert("span".into(), it.name.clone().into());
                    of.as_str()
                }
                None => it.name.as_str(),
            };
            put(
                &mut *out,
                json!({"ph": "X", "name": name, "cat": it.name, "pid": part.pid,
                "tid": it.tid, "ts": it.start_us, "dur": it.dur_us, "args": args}),
            )?;
        }
        for it in &part.counters {
            put(
                &mut *out,
                json!({"ph": "C", "name": it.name, "pid": part.pid, "tid": it.tid,
                "ts": it.at_us, "args": {"value": it.value}}),
            )?;
        }
        // A thread's CPU samples are on a track of their own beside it, each a slice named by
        // the function it was in, with its whole stack.
        for it in &part.samples {
            let tid = it.tid + (2 << 32);
            if named.insert((part.pid, tid)) {
                let thread = part.threads.iter().find(|(at, _)| *at == it.tid);
                let thread = thread.map_or_else(
                    // alloc: a dump, on demand
                    || format!("thread {} samples", it.tid),
                    // alloc: a dump, on demand
                    |it| format!("{} samples", it.1),
                );
                put(
                    &mut *out,
                    json!({"ph": "M", "name": "thread_name", "pid": part.pid, "tid": tid,
                    "args": {"name": thread}}),
                )?;
            }
            // A blocked sample on Linux has no stack (`sampler/linux.rs` says why): its span
            // says where it blocked.
            let name = it.stack.first().map_or("blocked (no stack)", String::as_str);
            put(
                &mut *out,
                json!({"ph": "X", "name": name, "cat": "sample", "pid": part.pid, "tid": tid,
                "ts": it.at_us, "dur": 1, "args": {"stack": it.stack.join("\n"), "span": it.span,
                "job": it.job, "fiber": it.fiber}}),
            )?;
        }
        for it in &part.fibers {
            // clone: a dump, on demand
            put(&mut *out, it.clone())?;
        }
    }
    let mut ours = json!({"start_us": start, "total_us": end.saturating_sub(start)});
    // The samples folded, outermost function first and `;` between, with how often each stack
    // was seen, most first: a flame graph's input, and text an agent reads.
    let mut folded: HashMap<String, u64, Global> = HashMap::new_in(Global);
    for it in parts.iter().flat_map(|part| &part.samples) {
        // alloc: a dump, on demand
        let stack: Vec<&str> = it.stack.iter().rev().map(String::as_str).collect();
        *folded.entry(stack.join(";")).or_default() += 1;
    }
    if !folded.is_empty() {
        // alloc: a dump, on demand
        let mut folded: Vec<(String, u64)> = folded.into_iter().collect();
        folded.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ours["samples"] = json!(folded);
    }
    let sampler: Vec<&str> =
        // alloc: a dump, on demand
        parts.iter().map(|it| it.sampler.as_str()).filter(|it| !it.is_empty()).collect();
    if !sampler.is_empty() {
        ours["sampler"] = json!(sampler);
    }
    write!(out, "\n],\"displayTimeUnit\":\"ms\",\"blaze\":{ours}}}")
}
