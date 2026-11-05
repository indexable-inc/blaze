//! Chrome trace event JSON (what Perfetto, chrome://tracing and speedscope open): spans as
//! complete (`X`) events, waits in category `wait`, polls in `poll`, counters as `C` events,
//! thread names as `M` metadata, CPU samples as instant events on their thread. Times are
//! microseconds from the profile's start. Output is canonical (sorted keys, rows in table
//! order), so the same profile gives the same bytes.

use crate::{
    json::{Json, str},
    profile::{Profile, SpanKind},
};

/// Microseconds, with the nanoseconds as the fraction.
fn us(ns: i64) -> Json {
    Json::Float(ns as f64 / 1000.0)
}

fn u(v: u64) -> Json {
    Json::Int(i128::from(v))
}

fn tid(t: u64) -> Json {
    u(t)
}

fn obj<'a>(pairs: impl IntoIterator<Item = (&'a str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub fn to_chrome(p: &Profile) -> String {
    let start = p.meta.start_ns;
    let pid = |process: u32| u(u64::from(process));
    let mut events: Vec<Json> = Vec::new();
    for t in &p.threads {
        events.push(obj([
            ("args", obj([("name", str(t.thread_name.as_deref().unwrap_or("")))])),
            ("name", str("thread_name")),
            ("ph", str("M")),
            ("pid", pid(t.process)),
            ("tid", tid(t.thread)),
        ]));
    }
    if let Some(t) = p.threads.first() {
        events.push(obj([
            ("args", obj([("name", str(&t.process_name))])),
            ("name", str("process_name")),
            ("ph", str("M")),
            ("pid", pid(t.process)),
        ]));
    }
    for s in &p.spans {
        let cat = match (s.kind, s.wait.is_some()) {
            (SpanKind::Wait, _) | (_, true) => "wait",
            (SpanKind::Poll, _) => "poll",
            (SpanKind::Fn, _) => "fn",
            (SpanKind::Span, _) => "span",
        };
        let mut args: Vec<(&str, Json)> = vec![("span_id", u(s.span_id))];
        if let Some(parent) = s.parent {
            args.push(("parent", u(parent)));
        }
        if let Some(v) = s.self_cpu_ns {
            args.push(("self_cpu_ns", Json::Int(i128::from(v))));
        }
        if let Some(v) = s.alloc_bytes {
            args.push(("alloc_bytes", u(v)));
        }
        if let Some(v) = &s.wait {
            args.push(("wait", str(v)));
        }
        if let Some(v) = &s.wait_on {
            args.push(("on", str(v)));
        }
        if let Some(v) = &s.wait_owner {
            args.push(("owner", str(v)));
        }
        if let Some(v) = s.task {
            args.push(("task", u(v)));
        }
        if let Some(fields) = &s.fields {
            for kv in fields.split('\x1f') {
                if let Some((k, v)) = kv.split_once('=') {
                    args.push((k, str(v)));
                }
            }
        }
        events.push(obj([
            ("args", obj(args)),
            ("cat", str(cat)),
            ("dur", us(s.dur_ns)),
            ("name", str(&s.name)),
            ("ph", str("X")),
            ("pid", pid(s.process)),
            ("tid", tid(s.thread)),
            ("ts", us(s.start_ns - start)),
        ]));
    }
    for c in &p.counters {
        events.push(obj([
            ("args", obj([("value", Json::Float(c.value))])),
            ("name", str(&c.name)),
            ("ph", str("C")),
            ("pid", pid(c.process)),
            ("ts", us(c.t_ns - start)),
        ]));
    }
    for s in &p.cpu_samples {
        let leaf = p
            .frames_of(s.stack)
            .first()
            .and_then(|f| p.frames.get(*f as usize))
            .map_or("?", |f| f.function.as_str());
        events.push(obj([
            ("cat", str(if s.on_cpu { "cpu" } else { "off-cpu" })),
            ("name", str(leaf)),
            ("ph", str("i")),
            ("pid", pid(s.process)),
            ("s", str("t")),
            ("tid", tid(s.thread)),
            ("ts", us(s.t_ns - start)),
        ]));
    }
    obj([
        ("displayTimeUnit", str("ns")),
        (
            "otherData",
            obj([("program", str(&p.meta.program)), ("producer", str(&p.meta.producer))]),
        ),
        ("traceEvents", Json::Arr(events)),
    ])
    .to_text()
}
