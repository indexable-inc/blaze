//! The tracing sink, a driver: each span's life (new, record, enter, exit, close) into the
//! recorder, with the thread's CPU time and allocation counts at enter and exit. Off, each
//! callback is one relaxed load.
//!
//! A span is a wait when it has a field `wait` (its kind), with `on` (what it waits for) and
//! `owner` (who will wake it); `kind = "poll"` and `task = <id>` mark an async poll.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_core::lock::Lock;
use blaze_trace::{
    Attributes, Event, Id, Metadata, Parent, Record, Sink,
    field::{Field, Visit},
};

use crate::{
    clock,
    core::{NewSpan, Usage},
    profile::SpanKind,
};

/// The sink; `blaze_trace::install(ix_prof::layer())` at the program's entry.
pub struct ProfLayer {
    _private: (),
}

static LAYER: ProfLayer = ProfLayer { _private: () };

pub fn layer() -> &'static ProfLayer {
    &LAYER
}

/// A span's id is its number in the profile, never reused. 0 is a span begun while off.
static NEXT: AtomicU64 = AtomicU64::new(1);

/// The handles of a span past its first (a `Span` cloned): it closes when the last drops.
/// Empty unless a span is cloned.
static EXTRA: Lock<BTreeMap<u64, u32>> = Lock::unrecorded("ix_prof.layer.extra", BTreeMap::new());

thread_local! {
    /// The spans this thread is inside, innermost last: a span's parent when it names none.
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

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

impl Sink for ProfLayer {
    fn enabled(&self, _: &'static Metadata<'static>) -> bool {
        crate::recording()
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        if !crate::recording() {
            return Id::from_u64(0);
        }
        let mine = NEXT.fetch_add(1, Ordering::Relaxed);
        crate::alloc::quiet(|| {
            let parent = match attrs.parent_of() {
                Parent::Explicit(id) => Some(id.into_u64()).filter(|it| *it != 0),
                Parent::Root => None,
                Parent::Current => ENTERED.with(|it| it.borrow().last().copied()),
            };
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
        Id::from_u64(mine)
    }

    fn record(&self, id: &Id, values: &Record<'_>) {
        let mine = id.into_u64();
        if !crate::recording() || mine == 0 {
            return;
        }
        crate::alloc::quiet(|| {
            let mut fields = Fields::default();
            values.record(&mut fields);
            crate::with_recorder(|r| r.span_record(mine, fields.all));
        });
    }

    fn event(&self, _: &Event<'_>) {}

    fn enter(&self, id: &Id) {
        let mine = id.into_u64();
        ENTERED.with(|it| it.borrow_mut().push(mine));
        if !crate::recording() || mine == 0 {
            return;
        }
        let (now, usage) = (clock::now_ns(), usage());
        crate::alloc::quiet(|| {
            crate::with_recorder(|r| r.span_enter(mine, clock::os_tid(), now, usage))
        });
    }

    fn exit(&self, id: &Id) {
        let mine = id.into_u64();
        ENTERED.with(|it| {
            let mut entered = it.borrow_mut();
            if let Some(at) = entered.iter().rposition(|it| *it == mine) {
                entered.remove(at);
            }
        });
        if !crate::recording() || mine == 0 {
            return;
        }
        let (now, usage) = (clock::now_ns(), usage());
        crate::alloc::quiet(|| {
            crate::with_recorder(|r| r.span_exit(mine, clock::os_tid(), now, usage))
        });
    }

    fn clone_span(&self, id: &Id) -> Id {
        *EXTRA.lock().entry(id.into_u64()).or_insert(0) += 1;
        *id
    }

    fn try_close(&self, id: Id) -> bool {
        let mine = id.into_u64();
        {
            let mut extra = EXTRA.lock();
            if let Some(more) = extra.get_mut(&mine) {
                *more -= 1;
                if *more == 0 {
                    extra.remove(&mine);
                }
                return false;
            }
        }
        if !crate::recording() || mine == 0 {
            return true;
        }
        let now = clock::now_ns();
        crate::alloc::quiet(|| crate::with_recorder(|r| r.span_close(mine, now)));
        true
    }
}
