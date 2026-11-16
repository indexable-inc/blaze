//! The core's promises: spans nest as the thread entered them, every field kind is given to
//! the sink as written, and a site no sink wants evaluates nothing.

extern crate std;

use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    Attributes, Event, Id, Level, Metadata, Off, Record, Sink,
    capture::{Capture, Captured, Counter, Kept},
    field::Empty,
};

/// The oracle of nesting: every span or event lies inside its parent's time (from the
/// parent's start to its end, or to now while it is open), and its parent began before it.
fn nested(records: &[Captured]) -> Result<(), String> {
    for it in records {
        let Some(parent) = it.parent else { continue };
        let Some(outer) = records.iter().find(|p| p.id == parent) else {
            return Err(format!("`{}` names a parent {parent} that was never kept", it.name));
        };
        let ends = |at: Option<u64>| at.unwrap_or(u64::MAX);
        if outer.start > it.start || ends(it.end) > ends(outer.end) {
            return Err(format!(
                "`{}` ({}..{:?}) is not inside its parent `{}` ({}..{:?})",
                it.name, it.start, it.end, outer.name, outer.start, outer.end
            ));
        }
    }
    Ok(())
}

/// What the nesting tests run: two spans, one inside the other, an event in each, an event
/// after the inner one ended, and a root span opened inside both.
fn nest() {
    let outer = crate::info_span!(target: "blaze", "outer").entered();
    crate::info!("in outer");
    {
        let _inner = crate::info_span!(target: "blaze", "inner", step = 1u64).entered();
        crate::info!("in inner");
        drop(crate::info_span!(parent: None, "root"));
    }
    crate::info!("after inner");
    drop(outer);
}

/// The invariant: a span or event is inside the span the thread entered last and has not left,
/// in its time and by its parent; `parent: None` is a root.
#[test]
fn spans_nest_as_the_thread_entered_them() {
    static CAPTURE: Capture<Counter> = Capture::new(Counter::new());
    crate::with_thread_sink(&CAPTURE, nest);
    let records = CAPTURE.records();
    nested(&records).unwrap();
    let id = |name: &str| records.iter().find(|it| it.name == name).map(|it| it.id);
    let parent_of_event = |message: &str| {
        records
            .iter()
            .find(|it| it.event && it.field("message") == Some(&Kept::Text(message.into())))
            .and_then(|it| it.parent)
    };
    assert_eq!(records.iter().find(|it| it.name == "outer").unwrap().parent, None);
    assert_eq!(records.iter().find(|it| it.name == "inner").unwrap().parent, id("outer"));
    assert_eq!(records.iter().find(|it| it.name == "root").unwrap().parent, None, "parent: None");
    assert_eq!(parent_of_event("in outer"), id("outer"));
    assert_eq!(parent_of_event("in inner"), id("inner"));
    assert_eq!(parent_of_event("after inner"), id("outer"), "left `inner` as its guard dropped");
    assert!(records.iter().filter(|it| !it.event).all(|it| it.end.is_some()), "every span closed: {records:?}");
}

/// A sink with a planted bug: it never leaves a span, so the thread stays inside every span it
/// entered.
struct NeverLeaves(Capture<Counter>);

impl Sink for NeverLeaves {
    fn enabled(&self, meta: &'static Metadata<'static>) -> bool {
        self.0.enabled(meta)
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        self.0.new_span(span)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        self.0.record(span, values);
    }

    fn event(&self, event: &Event<'_>) {
        self.0.event(event);
    }

    fn enter(&self, span: &Id) {
        self.0.enter(span);
    }

    fn exit(&self, _: &Id) {}

    fn try_close(&self, span: Id) -> bool {
        self.0.try_close(span)
    }
}

/// The control of [`spans_nest_as_the_thread_entered_them`]: with exits lost, the event after
/// `inner` ended is put inside `inner`, after its end, and the oracle says so.
#[test]
fn the_oracle_catches_a_sink_that_never_leaves_a_span() {
    static CAPTURE: NeverLeaves = NeverLeaves(Capture::new(Counter::new()));
    crate::with_thread_sink(&CAPTURE, nest);
    let found = nested(&CAPTURE.0.records()).unwrap_err();
    assert!(found.contains("after inner") || found.contains("event "), "{found}");
}

struct Shown;

impl core::fmt::Display for Shown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("shown")
    }
}

/// Every way of writing a field reaches the sink as written: numbers as themselves, `%` by
/// `Display`, `?` by `Debug`, `Empty` and `None` as no value until `record` gives one, the
/// shorthands by their names, and an event's message formatted with its arguments.
#[test]
fn every_field_is_given_as_written() {
    static CAPTURE: Capture<Counter> = Capture::new(Counter::new());
    let (krate, list, shown) = ("app", [1, 2], Shown);
    crate::with_thread_sink(&CAPTURE, || {
        let span = crate::info_span!(
            target: "blaze",
            "fields",
            n = -3i64,
            count = 7usize,
            ratio = 0.5,
            ok = true,
            wide = u128::MAX,
            label = "x",
            owned = String::from("y"),
            display = %shown,
            debug = ?list,
            later = Empty,
            none = None::<u64>,
            krate,
            %shown,
            ?list
        );
        span.record("later", 9u64).record("not a field", 1u64);
        crate::warn!(target: "blaze", step = 2u64, "made {} of {krate}", 3);
        crate::debug!(?list);
    });
    let span = CAPTURE.named("fields").unwrap();
    let text = |it: &str| Kept::Text(it.into());
    assert_eq!(
        span.fields,
        [
            ("n", Kept::I64(-3)),
            ("count", Kept::U64(7)),
            ("ratio", Kept::F64(0.5)),
            ("ok", Kept::Bool(true)),
            ("wide", Kept::U128(u128::MAX)),
            ("label", text("x")),
            ("owned", text("y")),
            ("display", text("shown")),
            ("debug", text("[1, 2]")),
            ("krate", text("app")),
            ("shown", text("shown")),
            ("list", text("[1, 2]")),
            ("later", Kept::U64(9)),
        ],
        "in the order given, `later` last as it was recorded, `none` and the unknown field never"
    );
    assert_eq!((span.target, span.level, span.event), ("blaze", Level::INFO, false));
    let events: Vec<_> = CAPTURE.records().into_iter().filter(|it| it.event).collect();
    assert_eq!(events[0].fields, [("step", Kept::U64(2)), ("message", text("made 3 of app"))]);
    assert_eq!(events[0].level, Level::WARN);
    assert!(events[0].name.starts_with("event ") && events[0].name.contains("tests.rs:"), "{}", events[0].name);
    assert_eq!(events[1].target, module_path!(), "a site's target is its module unless it names one");
    assert_eq!(events[1].fields, [("list", text("[1, 2]"))]);
}

/// Counts one evaluation in `count`.
fn evaluated(count: &AtomicU64) -> u64 {
    count.fetch_add(1, Ordering::Relaxed);
    1
}

/// Runs a span and an event whose two fields and two message arguments count their
/// evaluations in `count`: whether the span is none.
fn counted_sites(count: &AtomicU64) -> bool {
    let span = crate::info_span!("counted", value = evaluated(count), shown = %evaluated(count));
    crate::info!(value = evaluated(count), "{}", evaluated(count));
    span.is_none()
}

/// The invariant: a site no sink wants evaluates none of its fields and formats nothing, with
/// no sink at all or with a sink that wants nothing; its span is none.
#[test]
fn no_sink_evaluates_no_field() {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    // Nothing in this test binary installs an entry sink.
    std::thread::spawn(|| {
        assert!(crate::current().is_none());
        assert!(counted_sites(&COUNT), "no sink: no span");
        assert!(crate::with_thread_sink(&Off, || counted_sites(&COUNT)), "a sink that wants nothing: no span");
        assert_eq!(COUNT.load(Ordering::Relaxed), 0, "no field was evaluated");
    })
    .join()
    .unwrap();
}

/// The control of [`no_sink_evaluates_no_field`]: with a sink that wants the sites, each of
/// the four counted values is evaluated once, so the counter sees evaluation where it happens.
#[test]
fn the_oracle_catches_fields_evaluated_for_a_sink() {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static CAPTURE: Capture<Counter> = Capture::new(Counter::new());
    std::thread::spawn(|| {
        assert!(!crate::with_thread_sink(&CAPTURE, || counted_sites(&COUNT)));
        assert_eq!(COUNT.load(Ordering::Relaxed), 4);
    })
    .join()
    .unwrap();
}

/// A level's name in any case or its number reads back; `TRACE` is compiled in exactly when
/// debug assertions are on.
#[test]
fn levels_read_and_order_as_written() {
    assert_eq!("info".parse(), Ok(Level::INFO));
    assert_eq!("DEBUG".parse(), Ok(Level::DEBUG));
    assert_eq!("1".parse(), Ok(Level::ERROR));
    assert!("6".parse::<Level>().is_err() && "loud".parse::<Level>().is_err());
    assert!(Level::TRACE > Level::DEBUG && Level::WARN < Level::INFO && Level::ERROR < Level::WARN);
    assert_eq!(Level::compiled(Level::TRACE), cfg!(debug_assertions));
    assert!(Level::compiled(Level::DEBUG));
}
