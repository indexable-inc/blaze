//! A sink that keeps every span and event as plain data, its times from a clock passed in: a
//! test's oracle and a simulation's trace. One thread at a time: the span it is inside is the
//! one entered last on any thread.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{
    cell::UnsafeCell,
    fmt::{self, Write as _},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::{
    Attributes, Event, Id, Level, Metadata, Parent, Record, Sink,
    field::{Field, Visit},
};

/// Where a capture's ticks come from.
pub trait Clock: Sync {
    fn ticks(&self) -> u64;
}

/// A clock that counts: each read is one tick after the last, from 1. Deterministic, so two
/// runs of one test capture equal records.
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }
}

impl Default for Counter {
    fn default() -> Counter {
        Counter::new()
    }
}

impl Clock for Counter {
    fn ticks(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// A field's value as kept: a number as itself, anything else as its text (`Display` for `%`
/// and a message, `Debug` for `?`).
#[derive(Clone, Debug, PartialEq)]
pub enum Kept {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    I128(i128),
    U128(u128),
    Text(String),
}

/// One span or event kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Captured {
    /// From 1, in the order they began: a span's [`Id`].
    pub id: u64,
    pub name: &'static str,
    pub target: &'static str,
    pub level: Level,
    pub event: bool,
    /// The span it is inside; `None` at the root.
    pub parent: Option<u64>,
    /// Each field given, in the order given; a later `record` of one replaces it in place.
    pub fields: Vec<(&'static str, Kept)>,
    pub start: u64,
    /// When its last handle dropped (an event's is its start); `None` while open.
    pub end: Option<u64>,
    /// How many times it was entered.
    pub entered: u32,
}

impl Captured {
    /// The value of its field `name`.
    pub fn field(&self, name: &str) -> Option<&Kept> {
        self.fields.iter().find(|it| it.0 == name).map(|it| &it.1)
    }
}

struct State {
    kept: Vec<(Captured, u32)>,
    entered: Vec<u64>,
}

/// The sink: [`crate::with_thread_sink`] it for the code under test, then read
/// [`Capture::records`].
pub struct Capture<C: Clock> {
    clock: C,
    most: Level,
    state: Spin<State>,
}

impl<C: Clock> Capture<C> {
    /// Keeps every site compiled in.
    pub const fn new(clock: C) -> Capture<C> {
        Capture::at(clock, Level::TRACE)
    }

    /// Keeps the sites at `most` or less verbose.
    pub const fn at(clock: C, most: Level) -> Capture<C> {
        Capture { clock, most, state: Spin::new(State { kept: Vec::new(), entered: Vec::new() }) }
    }

    /// What it kept, in the order each began.
    pub fn records(&self) -> Vec<Captured> {
        // alloc: a test's copy of what was kept
        self.state.with(|state| state.kept.iter().map(|it| it.0.clone()).collect())
    }

    /// The first span or event named `name`.
    pub fn named(&self, name: &str) -> Option<Captured> {
        self.records().into_iter().find(|it| it.name == name)
    }

    fn parent(state: &State, parent: Parent) -> Option<u64> {
        match parent {
            Parent::Current => state.entered.last().copied(),
            Parent::Root => None,
            Parent::Explicit(id) => Some(id.into_u64()),
        }
    }

    fn keep(&self, meta: &'static Metadata<'static>, parent: Parent, fields: Vec<(&'static str, Kept)>, event: bool) -> u64 {
        self.state.with(|state| {
            let id = state.kept.len() as u64 + 1;
            let start = self.clock.ticks();
            let parent = Capture::<C>::parent(state, parent);
            state.kept.push((
                Captured {
                    id,
                    name: meta.name(),
                    target: meta.target(),
                    level: *meta.level(),
                    event,
                    parent,
                    fields,
                    start,
                    end: event.then_some(start),
                    entered: 0,
                },
                1,
            ));
            id
        })
    }
}

/// Reads values into [`Kept`]s: formats them here, before the capture's lock is taken, so a
/// value whose `Debug` records a span of its own does not wait on itself.
#[derive(Default)]
struct Gather(Vec<(&'static str, Kept)>);

impl Visit for Gather {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.push((field.name(), Kept::I64(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.push((field.name(), Kept::U64(value)));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.push((field.name(), Kept::F64(value)));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.push((field.name(), Kept::Bool(value)));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.0.push((field.name(), Kept::I128(value)));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.0.push((field.name(), Kept::U128(value)));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name(), Kept::Text(value.to_string())));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut text = String::new();
        // Writing to a `String` does not fail.
        _ = write!(text, "{value:?}");
        self.0.push((field.name(), Kept::Text(text)));
    }
}

impl<C: Clock> Sink for Capture<C> {
    fn enabled(&self, meta: &'static Metadata<'static>) -> bool {
        *meta.level() <= self.most
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let mut fields = Gather::default();
        span.record(&mut fields);
        Id::from_u64(self.keep(span.metadata(), span.parent_of(), fields.0, false))
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut fields = Gather::default();
        values.record(&mut fields);
        self.state.with(|state| {
            let Some((kept, _)) = state.kept.get_mut(span.into_u64() as usize - 1) else { return };
            for (name, value) in fields.0 {
                match kept.fields.iter_mut().find(|it| it.0 == name) {
                    Some(it) => it.1 = value,
                    None => kept.fields.push((name, value)),
                }
            }
        });
    }

    fn event(&self, event: &Event<'_>) {
        let mut fields = Gather::default();
        event.record(&mut fields);
        self.keep(event.metadata(), event.parent_of(), fields.0, true);
    }

    fn enter(&self, span: &Id) {
        self.state.with(|state| {
            state.entered.push(span.into_u64());
            if let Some((kept, _)) = state.kept.get_mut(span.into_u64() as usize - 1) {
                kept.entered += 1;
            }
        });
    }

    fn exit(&self, span: &Id) {
        self.state.with(|state| {
            if let Some(at) = state.entered.iter().rposition(|it| *it == span.into_u64()) {
                state.entered.remove(at);
            }
        });
    }

    fn clone_span(&self, span: &Id) -> Id {
        self.state.with(|state| {
            if let Some((_, refs)) = state.kept.get_mut(span.into_u64() as usize - 1) {
                *refs += 1;
            }
        });
        *span
    }

    fn try_close(&self, span: Id) -> bool {
        self.state.with(|state| {
            let Some((kept, refs)) = state.kept.get_mut(span.into_u64() as usize - 1) else {
                return false;
            };
            *refs -= 1;
            if *refs > 0 {
                return false;
            }
            kept.end = Some(self.clock.ticks());
            true
        })
    }
}

/// The capture's lock: held only for a push or a lookup, never while a value is formatted.
struct Spin<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is reached only through `with`, by the one thread that holds `held`.
unsafe impl<T: Send> Sync for Spin<T> {}

/// Rounds a wait for the capture's lock spins before it says the holder is stuck (rule
/// bounded-progress): far past any push.
const SPINS_MOST: u64 = 1 << 30;

impl<T> Spin<T> {
    const fn new(value: T) -> Spin<T> {
        Spin { held: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut spins = 0u64;
        while self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            assert!(spins < SPINS_MOST, "blaze_trace::capture: its lock was held past {SPINS_MOST} spins");
            core::hint::spin_loop();
        }
        // SAFETY: `held` was false and this thread set it, so no other reference to `value` is
        // live until it is cleared below.
        let out = f(unsafe { &mut *self.value.get() });
        self.held.store(false, Ordering::Release);
        out
    }
}
