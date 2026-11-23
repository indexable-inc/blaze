//! The core's promises: spans nest as the thread entered them, every field kind is given to
//! the sink as written, and a site no sink wants evaluates nothing.

extern crate std;

use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    Attributes, Event, Id, Level, Metadata, Off, Record, Sink,
    capture::{self, Capture, Counter},
    field::Empty,
    record,
};

/// The allocator every test hands what it makes: the test's root capability (D102).
type Heap = blaze_own::SharedAlloc<'static>;
type Kept = capture::Kept<Heap>;
type Captured = capture::Captured<Heap>;

fn heap() -> Heap {
    blaze_own::testing::heap().shared()
}

/// A capture that lives as long as the test binary, in the test's heap: what a sink must be.
fn capture() -> &'static Capture<Counter, Heap> {
    blaze_own::Box::leak_static(blaze_own::Box::new_in(Capture::new(Counter::new(), heap()), heap()))
}

/// A field's text as the capture keeps it.
fn text(it: &str) -> Kept {
    Kept::Text(blaze_own::String::from_str_in(it, heap()))
}

/// Whether two kept values hold the same thing. `Kept`'s derived `PartialEq` asks its allocator
/// to be `PartialEq` too, and `SharedAlloc` is not, so the tests compare the contents.
fn same(a: &Kept, b: &Kept) -> bool {
    match (a, b) {
        (Kept::I64(a), Kept::I64(b)) => a == b,
        (Kept::U64(a), Kept::U64(b)) => a == b,
        (Kept::F64(a), Kept::F64(b)) => a.to_bits() == b.to_bits(),
        (Kept::Bool(a), Kept::Bool(b)) => a == b,
        (Kept::I128(a), Kept::I128(b)) => a == b,
        (Kept::U128(a), Kept::U128(b)) => a == b,
        (Kept::Text(a), Kept::Text(b)) => a.as_str() == b.as_str(),
        _ => false,
    }
}

/// Asserts `got` keeps `want`'s fields: the same names in the same order, each with the same
/// contents.
#[track_caller]
fn assert_fields(got: &[(&str, Kept)], want: &[(&str, Kept)], why: &str) {
    let all_same =
        got.len() == want.len() && got.iter().zip(want).all(|((gn, gk), (wn, wk))| gn == wn && same(gk, wk));
    assert!(all_same, "{why}\n got: {got:?}\nwant: {want:?}");
}

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
    let capture = capture();
    crate::with_thread_sink(capture, nest);
    let records = capture.records();
    nested(&records).unwrap();
    let id = |name: &str| records.iter().find(|it| it.name == name).map(|it| it.id);
    let parent_of_event = |message: &str| {
        records
            .iter()
            .find(|it| it.event && it.field("message").and_then(|kept| kept.text()) == Some(message))
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
struct NeverLeaves(Capture<Counter, Heap>);

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
    let never: &'static NeverLeaves =
        blaze_own::Box::leak_static(blaze_own::Box::new_in(NeverLeaves(Capture::new(Counter::new(), heap())), heap()));
    crate::with_thread_sink(never, nest);
    let found = nested(&never.0.records()).unwrap_err();
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
    let capture = capture();
    let (krate, list, shown) = ("app", [1, 2], Shown);
    crate::with_thread_sink(capture, || {
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
    let span = capture.named("fields").unwrap();
    assert_fields(
        &span.fields,
        &[
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
        "in the order given, `later` last as it was recorded, `none` and the unknown field never",
    );
    assert_eq!((span.target, span.level, span.event), ("blaze", Level::INFO, false));
    let events: Vec<_> = capture.records().into_iter().filter(|it| it.event).collect();
    assert_fields(&events[0].fields, &[("step", Kept::U64(2)), ("message", text("made 3 of app"))], "the event's");
    assert_eq!(events[0].level, Level::WARN);
    assert!(events[0].name.starts_with("event ") && events[0].name.contains("tests.rs:"), "{}", events[0].name);
    assert_eq!(events[1].target, module_path!(), "a site's target is its module unless it names one");
    assert_fields(&events[1].fields, &[("list", text("[1, 2]"))], "the debug event's");
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
    let capture = capture();
    std::thread::spawn(move || {
        assert!(!crate::with_thread_sink(capture, || counted_sites(&COUNT)));
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

/// The only target the rings record in these tests, so the sink tests beside them are not
/// changed by the rings being installed.
const RING_TARGET: &str = "blaze_trace::ring_test";

/// The rings' clock in these tests: every read one tick later.
fn ring_clock() -> u64 {
    static TICK: AtomicU64 = AtomicU64::new(0);
    TICK.fetch_add(1, Ordering::Relaxed) + 1
}

/// The oracle of the rings' edges: each exit leaves the innermost span entered and not left,
/// at that span's depth, with a tick after its enter's; every span entered is left.
fn edges_nest(entries: &[record::Entry]) -> Result<(), String> {
    let mut open: Vec<(u64, u16, u64)> = Vec::new();
    for entry in entries {
        match *entry {
            record::Entry::Enter { depth, tick, span, .. } => {
                if usize::from(depth) != open.len() {
                    return Err(format!("span {span} entered at depth {depth} inside {} spans", open.len()));
                }
                open.push((span, depth, tick));
            }
            record::Entry::Exit { depth, tick, span, .. } => match open.pop() {
                Some((inner, at, began)) if inner == span && at == depth && began < tick => {}
                other => return Err(format!("span {span} left at depth {depth} where the innermost is {other:?}")),
            },
            _ => {}
        }
    }
    match open.is_empty() {
        true => Ok(()),
        false => Err(format!("spans {open:?} were never left")),
    }
}

/// Spans nest in the thread's ring as they were entered, and every field is in the entries as
/// written: a number as its raw word, text as its bytes.
#[test]
fn the_rings_hold_each_edge_and_field_as_written() {
    let rings: &'static record::Rings =
        blaze_own::Box::leak_static(blaze_own::Box::new_in(record::Rings::new_in(8, 256, heap()).unwrap(), heap()));
    crate::install_rings(crate::RingConfig {
        rings,
        wants: |meta| meta.target() == RING_TARGET,
        clock: record::Clock::Sim(ring_clock),
        limits: crate::TraceLimits::SHIPPED,
    })
    .unwrap();
    std::thread::spawn(move || {
        let outer = crate::info_span!(target: RING_TARGET, "outer").entered();
        {
            let _inner = crate::info_span!(target: RING_TARGET, "inner", n = 7u64, name = %"a text").entered();
        }
        let first = outer.id().unwrap().into_u64();
        drop(outer);
        let (lane, from) = record::span_parts(first).unwrap();
        let mut entries = Vec::new();
        rings
            .drain::<{ record::WORDS }>(crate::ring::Lane(lane), from, |got| match got {
                // An enter naming span 0 opened its span: the span's id is the entry's own.
                crate::ring::Got::Entry { n, words } => entries.push(match record::decode(words) {
                    record::Entry::Enter { site, depth, tick, span: 0 } => {
                        record::Entry::Enter { site, depth, tick, span: record::span_id(lane, n) }
                    }
                    other => other,
                }),
                crate::ring::Got::Lost { from, count } => panic!("entries {from}..+{count} lost in a ring of 256"),
            })
            .unwrap();
        edges_nest(&entries).unwrap();
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|it| match *it {
                record::Entry::Enter { site, .. } => record::site(site).map(Metadata::name),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["outer", "inner"]);
        let fields: Vec<record::Raw> = entries
            .iter()
            .filter_map(|it| match *it {
                record::Entry::Field { value, .. } => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(fields, [record::Raw::U64(7), record::Raw::Text { kept: 6, cut: 0 }]);
        let text: Vec<u8> = entries
            .iter()
            .filter_map(|it| match *it {
                record::Entry::Text { len, bytes } => Some(bytes[..usize::from(len)].to_vec()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(text, b"a text");
    })
    .join()
    .unwrap();
}

/// The control of [`edges_nest`]: entries whose spans are left in the wrong order, or never,
/// are caught.
#[test]
fn the_oracle_catches_edges_left_out_of_order() {
    use record::Entry::{Enter, Exit};
    let crossed = [
        Enter { site: 1, depth: 0, tick: 1, span: 10 },
        Enter { site: 2, depth: 1, tick: 2, span: 11 },
        Exit { site: 1, depth: 0, tick: 3, span: 10 },
        Exit { site: 2, depth: 1, tick: 4, span: 11 },
    ];
    assert!(edges_nest(&crossed).is_err());
    assert!(edges_nest(&crossed[..2]).is_err());
    assert!(edges_nest(&[crossed[0], Exit { site: 1, depth: 0, tick: 5, span: 10 }]).is_ok());
}
