//! The tracing layer, a driver: each span's life (new, record, enter, exit, close) into the
//! recorder, with the thread's CPU time and allocation counts at enter and exit. Off, each
//! callback is one relaxed load.
//!
//! A span is a wait when it has a field `wait` (its kind), with `on` (what it waits for) and
//! `owner` (who will wake it); `kind = "poll"` and `task = <id>` mark an async poll.

use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

use crate::{
    clock,
    core::{NewSpan, Usage},
    profile::SpanKind,
};

/// The layer; `tracing_subscriber::registry().with(ix_prof::layer())`.
pub struct ProfLayer {
    _private: (),
}

pub fn layer() -> ProfLayer {
    ProfLayer { _private: () }
}

/// The span's number in the profile: tracing's ids are reused once a span closes.
struct Mine(u64);

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Fields {
    all: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.all.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.all.push((field.name().to_owned(), format!("{value:?}")));
    }
}

fn usage() -> Usage {
    let alloc = crate::alloc::thread_usage();
    Usage {
        cpu_ns: clock::thread_cpu_ns(),
        alloc_bytes: alloc.map(|it| it.0),
        allocs: alloc.map(|it| it.1),
        freed_bytes: alloc.map(|it| it.2),
    }
}

impl<S> Layer<S> for ProfLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !crate::recording() {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        crate::alloc::quiet(|| {
            let mine = NEXT.fetch_add(1, Ordering::Relaxed);
            let parent = span.parent().and_then(|p| p.extensions().get::<Mine>().map(|it| it.0));
            span.extensions_mut().insert(Mine(mine));
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            let meta = attrs.metadata();
            let mut new = NewSpan {
                id: mine,
                parent,
                thread: clock::os_tid(),
                name: meta.name().to_owned(),
                target: meta.target().to_owned(),
                file: meta.file().map(str::to_owned),
                line: meta.line(),
                ..NewSpan::default()
            };
            for (k, v) in fields.all {
                match k.as_str() {
                    "wait" => new.wait = Some(v),
                    "on" => new.wait_on = Some(v),
                    "owner" => new.wait_owner = Some(v),
                    "task" => new.task = v.parse().ok(),
                    "kind" if SpanKind::parse(&v).is_some() => new.kind = SpanKind::parse(&v),
                    _ => new.fields.push((k, v)),
                }
            }
            let name = std::thread::current().name().map(str::to_owned);
            let now = clock::now_ns();
            crate::with_recorder(|r| {
                r.thread(new.thread, name.as_deref());
                r.span_new(new, now);
            });
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if !crate::recording() {
            return;
        }
        let Some(mine) = ctx.span(id).and_then(|s| s.extensions().get::<Mine>().map(|it| it.0))
        else {
            return;
        };
        crate::alloc::quiet(|| {
            let mut fields = Fields::default();
            values.record(&mut fields);
            crate::with_recorder(|r| r.span_record(mine, fields.all));
        });
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if !crate::recording() {
            return;
        }
        let Some(mine) = ctx.span(id).and_then(|s| s.extensions().get::<Mine>().map(|it| it.0))
        else {
            return;
        };
        let (now, usage) = (clock::now_ns(), usage());
        crate::alloc::quiet(|| {
            crate::with_recorder(|r| r.span_enter(mine, clock::os_tid(), now, usage))
        });
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if !crate::recording() {
            return;
        }
        let (now, usage) = (clock::now_ns(), usage());
        let Some(mine) = ctx.span(id).and_then(|s| s.extensions().get::<Mine>().map(|it| it.0))
        else {
            return;
        };
        crate::alloc::quiet(|| {
            crate::with_recorder(|r| r.span_exit(mine, clock::os_tid(), now, usage))
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if !crate::recording() {
            return;
        }
        let now = clock::now_ns();
        let Some(mine) = ctx.span(&id).and_then(|s| s.extensions().get::<Mine>().map(|it| it.0))
        else {
            return;
        };
        crate::alloc::quiet(|| crate::with_recorder(|r| r.span_close(mine, now)));
    }
}
