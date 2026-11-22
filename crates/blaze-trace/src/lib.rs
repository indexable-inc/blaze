//! blaze's spans and events: the one home of tracing in blaze (docs/v2/plan/design/no-tracing.md,
//! docs/v2/plan/third-party.md's `tracing` row). The `tracing` and `tracing-subscriber` crates
//! are gone; every crate writes these macros and the flight recorder (blaze-profile's
//! `Recorder`) is the sink that keeps what they say.
//!
//! - A site is static data, made by the macro where it is written: its name, target, level,
//!   file, line and field names ([`Metadata`]), and one state word ([`Site`]): its id in the
//!   process's site table and who records it.
//! - **The recording path** ([`record`]): a span's edges and an event are fixed 32-byte
//!   entries in the calling thread's ring ([`ring`], the one home of the rings, blaze-watch's
//!   too), timed by the CPU's counter. No `dyn` call, no allocation, no lock, no formatting but
//!   of text fields; the reader formats, later.
//! - **A sink** ([`Sink`]) is the other consumer: a span or an event handed to it as it happens,
//!   its fields' values borrowed ([`Value`]). It is how blaze-profile's flight recorder and the
//!   tests read spans until the flight recorder drains the rings; a site the rings record does
//!   not go to a sink.
//! - The macros have the call shape crates already wrote (`info_span!(target: "blaze", "name",
//!   krate = %k, unit)`, `warn!(%dir, "text {x}")`, `field::Empty` then `span.record(..)`), so
//!   moving a crate here changes its import and nothing else.
//!
//! # What this crate is (D170, C7): the profile ring's driver, over cores
//!
//! The crate is the driver of the process's profile ring, the second of the three statics C3
//! allows (the allocator, the profile ring, the signal handler; each named and capped): it holds
//! that one static ([`record::PROFILE`], with its per-thread half [`record`]'s `THREAD`), the
//! CPU counter read that is the ring's clock, and the C runtime's thread-exit hook that gives a
//! lane back. Nothing else in it has state outside a value: its cores are [`ring`] (the rings'
//! protocol and the drain's step machine, `#![blaze::sans_io]`) and the field values
//! ([`Value`], `#![blaze::sans_io]`); [`capture`] is a sink over an allocator and a clock it
//! is handed.
//!
//! # The one ambient hook
//!
//! Rule 75 (nothing ambient) says the clock, the environment and IO are passed in. A span site
//! has no handle in scope (it is a line in a parser or a type checker, 300 of them), so what
//! records it is reached ambiently, through the profile ring's static alone ([`record`]'s
//! "The process's profile ring": the thread's lane producer and its own sink, when it set one
//! with [`set_thread`], and what the program's entry installed once, [`install_rings`] and
//! [`install`], never replaced). Every input the entry installs is a value it made: the rings
//! from its allocator, the clock and the caps in the [`RingConfig`], the sink. A thread a pool or a test spawns reaches the entry's rings and sink with no step
//! of its own: the flight recorder must see every thread. A task that wants its own record (a
//! test, a simulation) sets its thread's sink and gets the entry's back when it is done
//! ([`with_thread_sink`]). bstd's runtime has no thread-local of its own yet; when its
//! current-worker pointer lands, this state moves into it, and code with a `bstd::Cx` records
//! through it.
//!
//! # What a site costs
//!
//! Recorded by nobody: one relaxed load of the site's state and a branch; no field is
//! evaluated. Recorded by the rings: per edge, a counter read and one 32-byte slot written.
//! `trace!` and `trace_span!` compile out of builds without debug assertions
//! ([`Level::COMPILED`]). Evidence: [`tests::no_sink_evaluates_no_field`], `examples/cost.rs`.
//!
//! Measured 2026-11-15, one run, this machine (an arm64 Mac), `examples/cost.rs` in release,
//! ns per span (open, enter, exit, close), 2M spans a thread; first run, then after the fixes:
//!
//! | case                                       | first, 1 / 8 thr | now, 1 / 8 thr |
//! |--------------------------------------------|-----------------:|---------------:|
//! | empty loop                                 |                  |    0.23        |
//! | blaze_trace, nothing recording             |     2.52 /  2.96 |    0.93 /  1.19 |
//! | tracing, no subscriber                     |     1.83 /  1.89 |    1.75 /  1.81 |
//! | tracing, a subscriber keeping nothing      |     6.23 / 54.61 |    5.23 / 29.52 |
//! | blaze_trace, rings                         |    10.34 / 11.60 |    6.06 /  6.73 |
//! | tracing, a subscriber writing the rings    |    13.72 / 97.96 |   10.53 / 27.84 |
//! | floor: two counter reads, two slot writes  |     5.94         |    3.95        |
//!
//! Now: recording 1.74x faster than `tracing` on one thread, 4.1x on eight; 1.53x the floor
//! (rule 67 wants 1.1x). Disabled: 0.53x `tracing`'s. The fixes, each found by the ladder in
//! `examples/cost.rs` and the disassembly: a span finds its thread's state once, at its enter,
//! and its guard keeps it; the rings' config is taken once, at open; the tick is the counter with
//! no branch (the simulation clock is the `sim-clock` feature); a guard's drop is one test of
//! the thread pointer it holds, the edge out of line (it was an out-of-line drop with a
//! six-register prologue, calling `stamp`, `span_id` and `Site::id`, at every site); the push
//! writes through the ring's own words without bounds checks; and the ring fences once a block
//! of 64 entries, not once a slot ([`ring`]: that fence was 1.2 ns of a 2.6 ns push).

#![no_std]
#![feature(thread_local, allocator_api)]
// `#![blaze::sans_io]` on the core modules (`ring`, `value`); inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

// Only `value`'s `ToValue` impls for `alloc`'s `String`, `Cow`, `Box`, `Arc` and `Rc` name it:
// callers give such values as fields today (the module says when they go).
extern crate alloc;

use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

pub mod capture;
mod macros;
pub mod record;
pub mod ring;
mod value;

pub use record::{RingConfig, RingsRefused, Site, TraceLimits, install_rings};
pub use value::{ToValue, Value};

#[cfg(test)]
mod tests;

/// A site's or a filter's severity. More verbose is greater: `TRACE > DEBUG > INFO > WARN >
/// ERROR`, so "kept at INFO" is `level <= Level::INFO`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level(u8);

impl Level {
    pub const ERROR: Level = Level(1);
    pub const WARN: Level = Level(2);
    pub const INFO: Level = Level(3);
    pub const DEBUG: Level = Level(4);
    pub const TRACE: Level = Level(5);
    /// Below every level: no site is at it.
    pub const NONE: Level = Level(0);

    /// The most verbose level a site is compiled at: `TRACE` with debug assertions, `DEBUG`
    /// without (what `tracing`'s `release_max_level_debug` was).
    pub const COMPILED: Level = if cfg!(debug_assertions) { Level::TRACE } else { Level::DEBUG };

    /// Whether a site at `level` is compiled in: what each macro asks first, a constant.
    #[inline(always)]
    pub const fn compiled(level: Level) -> bool {
        level.0 <= Level::COMPILED.0
    }

    /// `TRACE` to `ERROR`.
    pub const fn as_str(self) -> &'static str {
        match self.0 {
            1 => "ERROR",
            2 => "WARN",
            3 => "INFO",
            4 => "DEBUG",
            5 => "TRACE",
            _ => "NONE",
        }
    }
}

impl fmt::Debug for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A text that names no level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoLevel;

impl core::str::FromStr for Level {
    type Err = NoLevel;

    /// A level's name in any case (`info`, `INFO`) or its number, `1` (`ERROR`) to `5` (`TRACE`).
    fn from_str(text: &str) -> Result<Level, NoLevel> {
        let named = [Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG, Level::TRACE];
        if let Some(level) = named.into_iter().find(|it| it.as_str().eq_ignore_ascii_case(text)) {
            return Ok(level);
        }
        match text.parse::<u8>() {
            Ok(n @ 1..=5) => Ok(Level(n)),
            _ => Err(NoLevel),
        }
    }
}

/// Whether a site is a span's or an event's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kind(u8);

impl Kind {
    pub const SPAN: Kind = Kind(0);
    pub const EVENT: Kind = Kind(1);
}

pub mod field {
    //! A site's fields: their names, in the order the macro wrote them, and the visitor a sink
    //! reads values with.

    use core::fmt;

    pub use crate::value::{Empty, ToValue, Value};

    /// One field of a site: its name and its place among the site's fields.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Field {
        name: &'static str,
        index: usize,
    }

    impl Field {
        pub const fn name(&self) -> &'static str {
            self.name
        }

        /// Its place in its site's [`FieldSet`], from 0.
        pub const fn index(&self) -> usize {
            self.index
        }
    }

    impl fmt::Display for Field {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.name)
        }
    }

    /// The names of a site's fields. An event's message is its field named `message`, last.
    #[derive(Clone, Copy, Debug)]
    pub struct FieldSet {
        names: &'static [&'static str],
    }

    impl FieldSet {
        pub const fn new(names: &'static [&'static str]) -> FieldSet {
            FieldSet { names }
        }

        /// The field named `name`.
        pub fn field(&self, name: &str) -> Option<Field> {
            let index = self.names.iter().position(|it| *it == name)?;
            Some(Field { name: self.names[index], index })
        }

        pub fn iter(&self) -> impl Iterator<Item = Field> + '_ {
            self.names.iter().enumerate().map(|(index, name)| Field { name, index })
        }

        pub const fn len(&self) -> usize {
            self.names.len()
        }

        /// The field at `index`.
        pub fn at(&self, index: usize) -> Option<Field> {
            Some(Field { name: self.names.get(index)?, index })
        }
    }

    /// How a sink reads values: a number as itself, text as `&str`, anything else through
    /// `Debug` (a `Display` value comes as a `Debug` that prints its `Display`). Each typed
    /// method falls back to [`Visit::record_debug`].
    pub trait Visit {
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.record_debug(field, &value);
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.record_debug(field, &value);
        }

        fn record_f64(&mut self, field: &Field, value: f64) {
            self.record_debug(field, &value);
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.record_debug(field, &value);
        }

        fn record_i128(&mut self, field: &Field, value: i128) {
            self.record_debug(field, &value);
        }

        fn record_u128(&mut self, field: &Field, value: u128) {
            self.record_debug(field, &value);
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.record_debug(field, &value);
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug);
    }
}

use field::{Field, FieldSet, Visit};

/// A site: what a span or an event is, fixed where the macro is written. A sink keys what it
/// keeps of a site by its address (`&'static Metadata`), which is unique and lives as long as
/// the program.
#[derive(Debug)]
pub struct Metadata<'a> {
    name: &'a str,
    target: &'a str,
    level: Level,
    file: Option<&'a str>,
    line: Option<u32>,
    module_path: Option<&'a str>,
    fields: FieldSet,
    kind: Kind,
}

impl<'a> Metadata<'a> {
    #[expect(clippy::too_many_arguments, reason = "one argument a part of a site, as the macros write them")]
    pub const fn new(
        name: &'a str,
        target: &'a str,
        level: Level,
        file: Option<&'a str>,
        line: Option<u32>,
        module_path: Option<&'a str>,
        fields: FieldSet,
        kind: Kind,
    ) -> Metadata<'a> {
        Metadata { name, target, level, file, line, module_path, fields, kind }
    }

    /// A span's name as written; an event's is `event <file>:<line>`.
    pub const fn name(&self) -> &'a str {
        self.name
    }

    /// What it names as its target: its module's path unless it wrote `target: ..`.
    pub const fn target(&self) -> &'a str {
        self.target
    }

    pub const fn level(&self) -> &Level {
        &self.level
    }

    pub const fn file(&self) -> Option<&'a str> {
        self.file
    }

    pub const fn line(&self) -> Option<u32> {
        self.line
    }

    pub const fn module_path(&self) -> Option<&'a str> {
        self.module_path
    }

    pub const fn fields(&self) -> &FieldSet {
        &self.fields
    }

    pub const fn is_span(&self) -> bool {
        self.kind.0 == Kind::SPAN.0
    }

    pub const fn is_event(&self) -> bool {
        self.kind.0 == Kind::EVENT.0
    }
}

pub mod span {
    //! A span's id and what a sink is given as one begins or is given more fields.

    pub use crate::{Attributes, EnteredSpan, Id, Record, Span};
}

/// A span's id, as its sink made it: the sink's to say what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(u64);

impl Id {
    pub const fn from_u64(id: u64) -> Id {
        Id(id)
    }

    pub const fn into_u64(&self) -> u64 {
        self.0
    }
}

/// What a span or an event is inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    /// The innermost span the thread is in: the sink knows it ([`Sink::enter`]).
    Current,
    /// No span.
    Root,
    /// That span.
    Explicit(Id),
}

impl Parent {
    /// The parent `parent: ..` names in a macro: `None` for a root, or a span or its id.
    pub fn of(parent: impl Into<ParentOf>) -> Parent {
        parent.into().0
    }
}

/// What `parent: ..` may name ([`Parent::of`]).
pub struct ParentOf(Parent);

impl From<Option<Id>> for ParentOf {
    fn from(id: Option<Id>) -> ParentOf {
        ParentOf(id.map_or(Parent::Root, Parent::Explicit))
    }
}

impl From<Id> for ParentOf {
    fn from(id: Id) -> ParentOf {
        ParentOf(Parent::Explicit(id))
    }
}

impl From<&Id> for ParentOf {
    fn from(id: &Id) -> ParentOf {
        ParentOf(Parent::Explicit(*id))
    }
}

impl From<&Span> for ParentOf {
    fn from(span: &Span) -> ParentOf {
        span.id().into()
    }
}

impl From<&EnteredSpan> for ParentOf {
    fn from(span: &EnteredSpan) -> ParentOf {
        span.id().into()
    }
}

/// The values of a site's fields in its field order: `None` for one not given (`field::Empty`
/// or an `Option` that was `None`).
pub type Values<'a> = &'a [Option<Value<'a>>];

fn visit(meta: &Metadata<'_>, values: Values<'_>, visitor: &mut dyn Visit) {
    for (index, value) in values.iter().enumerate() {
        if let (Some(value), Some(field)) = (value, meta.fields().at(index)) {
            value.visit(&field, visitor);
        }
    }
}

/// A span beginning, as its sink is given it.
pub struct Attributes<'a> {
    meta: &'static Metadata<'static>,
    values: Values<'a>,
    parent: Parent,
}

impl<'a> Attributes<'a> {
    pub const fn new(meta: &'static Metadata<'static>, parent: Parent, values: Values<'a>) -> Attributes<'a> {
        Attributes { meta, values, parent }
    }

    pub const fn metadata(&self) -> &'static Metadata<'static> {
        self.meta
    }

    /// Whether it is inside no span (`parent: None`).
    pub fn is_root(&self) -> bool {
        self.parent == Parent::Root
    }

    /// Whether it is inside the thread's innermost span.
    pub fn is_contextual(&self) -> bool {
        self.parent == Parent::Current
    }

    /// The span it named as its parent (`parent: span`).
    pub fn parent(&self) -> Option<&Id> {
        match &self.parent {
            Parent::Explicit(id) => Some(id),
            _ => None,
        }
    }

    pub const fn parent_of(&self) -> Parent {
        self.parent
    }

    /// Each value given, in the site's field order.
    pub fn record(&self, visitor: &mut dyn Visit) {
        visit(self.meta, self.values, visitor);
    }

    /// The values in the site's field order, `None` for one not given.
    pub const fn values(&self) -> Values<'a> {
        self.values
    }
}

/// An event, as its sink is given it. Its message is its field `message`.
pub struct Event<'a> {
    meta: &'static Metadata<'static>,
    values: Values<'a>,
    parent: Parent,
}

impl<'a> Event<'a> {
    pub const fn new(meta: &'static Metadata<'static>, parent: Parent, values: Values<'a>) -> Event<'a> {
        Event { meta, values, parent }
    }

    /// Gives the event to `sink`.
    pub fn dispatch(sink: &dyn Sink, meta: &'static Metadata<'static>, parent: Parent, values: Values<'_>) {
        sink.event(&Event { meta, values, parent });
    }

    /// Records an event of `site` as [`Site::how`] decided: what each event macro does.
    #[inline]
    pub fn emit(site: &'static Site, how: record::How, parent: Parent, values: Values<'_>) {
        match how {
            record::How::Ring(config, id) => record::event(config, id, parent, values),
            record::How::Sink(sink) => Event::dispatch(sink, site.metadata(), parent, values),
        }
    }

    pub const fn metadata(&self) -> &'static Metadata<'static> {
        self.meta
    }

    pub fn is_root(&self) -> bool {
        self.parent == Parent::Root
    }

    pub fn is_contextual(&self) -> bool {
        self.parent == Parent::Current
    }

    pub fn parent(&self) -> Option<&Id> {
        match &self.parent {
            Parent::Explicit(id) => Some(id),
            _ => None,
        }
    }

    pub const fn parent_of(&self) -> Parent {
        self.parent
    }

    pub fn record(&self, visitor: &mut dyn Visit) {
        visit(self.meta, self.values, visitor);
    }

    pub const fn values(&self) -> Values<'a> {
        self.values
    }
}

/// A value given to an open span after it began (`span.record(name, value)`).
pub struct Record<'a> {
    field: Field,
    value: Value<'a>,
}

impl<'a> Record<'a> {
    pub const fn new(field: Field, value: Value<'a>) -> Record<'a> {
        Record { field, value }
    }

    pub const fn field(&self) -> &Field {
        &self.field
    }

    pub const fn value(&self) -> &Value<'a> {
        &self.value
    }

    pub fn record(&self, visitor: &mut dyn Visit) {
        self.value.visit(&self.field, visitor);
    }
}

/// What keeps spans and events: the flight recorder in blaze, [`capture::Capture`] in a test.
/// Called on the thread the span or event happens on; a span's [`Sink::try_close`] on the
/// thread its last handle drops on.
pub trait Sink: Sync {
    /// Whether it keeps `meta`'s spans or events: the entry's sink is asked once a site (the
    /// answer kept in the site's state), a thread's own sink at every call, before any field is
    /// evaluated.
    fn enabled(&self, meta: &'static Metadata<'static>) -> bool;

    /// A span begins: its id, for the calls about it that follow.
    fn new_span(&self, span: &Attributes<'_>) -> Id;

    /// The open span `span` was given a value.
    fn record(&self, span: &Id, values: &Record<'_>);

    fn event(&self, event: &Event<'_>);

    /// The thread is now inside `span`.
    fn enter(&self, span: &Id);

    /// The thread left `span`.
    fn exit(&self, span: &Id);

    /// Another handle of `span` was made (a [`Span`] cloned): the id that handle holds.
    fn clone_span(&self, span: &Id) -> Id {
        *span
    }

    /// A handle of `span` dropped: whether that was its last, so it closed.
    fn try_close(&self, span: Id) -> bool {
        _ = span;
        false
    }
}

/// The sink that keeps nothing: what a thread sets to record nothing while the entry's sink is
/// installed.
pub struct Off;

impl Sink for Off {
    fn enabled(&self, _: &'static Metadata<'static>) -> bool {
        false
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id(0)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn event(&self, _: &Event<'_>) {}

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

// --- The one ambient hook ---------------------------------------------------------------

/// The entry's sink was installed already: a process has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installed;

/// Makes `sink` the process's: every thread with no sink of its own records into it the sites
/// the rings do not. The program's entry calls it once, before it starts work; a second call
/// is refused and changes nothing.
pub fn install(sink: &'static dyn Sink) -> Result<(), Installed> {
    record::PROFILE.entry.set(sink)?;
    record::decide_all();
    Ok(())
}

/// The entry's sink, kept in the profile ring's static ([`record::PROFILE`]).
#[inline]
fn entry() -> Option<&'static dyn Sink> {
    record::PROFILE.entry.get().copied()
}

/// Sets the calling thread's own sink (`None`: the entry's again): the one it had. While any
/// thread has one, every site takes the sink path, which asks the calling thread's sink.
pub fn set_thread(sink: Option<&'static dyn Sink>) -> Option<&'static dyn Sink> {
    let had = record::THREAD.sink.replace(sink);
    match (had.is_some(), sink.is_some()) {
        (false, true) => {
            if record::PROFILE.thread_sinks.fetch_add(1, Ordering::AcqRel) == 0 {
                record::decide_all();
            }
        }
        (true, false) => {
            if record::PROFILE.thread_sinks.fetch_sub(1, Ordering::AcqRel) == 1 {
                record::decide_all();
            }
        }
        _ => {}
    }
    had
}

/// Runs `f` with `sink` as the calling thread's, then gives the thread back the one it had,
/// also when `f` panics.
pub fn with_thread_sink<R>(sink: &'static dyn Sink, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static dyn Sink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_thread(self.0);
        }
    }
    let _restore = Restore(set_thread(Some(sink)));
    f()
}

/// The sink the calling thread records into: its own, else the entry's.
#[inline]
pub fn current() -> Option<&'static dyn Sink> {
    match record::THREAD.sink.get() {
        Some(sink) => Some(sink),
        None => entry(),
    }
}

/// The sink to give `meta`'s span or event to, if any wants it.
#[inline]
pub fn enabled(meta: &'static Metadata<'static>) -> Option<&'static dyn Sink> {
    let sink = current()?;
    if sink.enabled(meta) { Some(sink) } else { None }
}

// --- Spans ------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Open {
    sink: &'static dyn Sink,
    id: Id,
    meta: &'static Metadata<'static>,
}

/// A sink span's edges: out of line, so a guard's drop inlined at every site is the ring's
/// stores or nothing.
#[cold]
#[inline(never)]
fn sink_enter(open: &Open) {
    open.sink.enter(&open.id);
}

#[cold]
#[inline(never)]
fn sink_exit(open: &Open) {
    open.sink.exit(&open.id);
}

/// Who records a span.
enum Inner {
    None,
    /// The rings: its site, the rings' config (taken once, at open) and its span id, 0 until
    /// its first entry.
    Ring { site: &'static Site, config: &'static record::RingConfig, span: AtomicU64 },
    Sink(Open),
}

/// A handle of an open span: it closes when its last handle drops. Made by the span macros;
/// none when nothing records it, and then every call on it does nothing.
pub struct Span {
    inner: Inner,
}

impl Span {
    /// The span nothing records.
    #[inline(always)]
    pub const fn none() -> Span {
        Span { inner: Inner::None }
    }

    /// Begins a span of `site` as [`Site::how`] decided: what the span macros do. A ring span
    /// with no field and no named parent writes nothing yet: its first entry is its enter.
    #[inline]
    pub fn open(site: &'static Site, how: record::How, parent: Parent, values: Values<'_>) -> Span {
        match how {
            record::How::Ring(config, id) => {
                let span = if parent == Parent::Current && values.iter().all(Option::is_none) {
                    0
                } else {
                    record::open(config, id, parent, values)
                };
                Span { inner: Inner::Ring { site, config, span: AtomicU64::new(span) } }
            }
            record::How::Sink(sink) => Span::new(sink, site.metadata(), parent, values),
        }
    }

    /// Begins a span of `meta` in `sink`.
    pub fn new(sink: &'static dyn Sink, meta: &'static Metadata<'static>, parent: Parent, values: Values<'_>) -> Span {
        let id = sink.new_span(&Attributes { meta, values, parent });
        Span { inner: Inner::Sink(Open { sink, id, meta }) }
    }

    /// Enters it on the calling thread: for a ring span, the thread's state its exit takes
    /// (null otherwise), so the span finds its thread once.
    #[inline(always)]
    fn edge_in(&self) -> *const record::Thread {
        match &self.inner {
            Inner::None => core::ptr::null(),
            Inner::Ring { site, config, span } => {
                let (given, here) = record::enter(config, site.id(), span.load(Ordering::Relaxed));
                span.store(given, Ordering::Relaxed);
                here
            }
            Inner::Sink(open) => {
                sink_enter(open);
                // Not null, so the guard leaves it; a sink's exit never reads it.
                core::ptr::dangling()
            }
        }
    }

    /// Leaves it on the calling thread; `here` is what its [`Span::edge_in`] returned.
    #[inline(always)]
    fn edge_out(&self, here: *const record::Thread) {
        match &self.inner {
            Inner::None => {}
            // SAFETY: a ring span's `edge_in` returned `here` on this thread: the guards that
            // hold it are not `Send`.
            Inner::Ring { site, config, span } => unsafe {
                record::exit(config, here, site.id(), span.load(Ordering::Relaxed));
            },
            Inner::Sink(open) => sink_exit(open),
        }
    }

    /// A guard's drop: out of line, so the guard's own drop, inlined at every site, is one test
    /// of `here`, null only for a span nothing records (a register, where the span's tag would
    /// be a load of the guard just stored).
    #[inline(never)]
    fn leave(&self, here: *const record::Thread) {
        self.edge_out(here);
    }

    /// Enters it on this thread until the guard drops.
    #[inline(always)]
    pub fn entered(self) -> EnteredSpan {
        let here = self.edge_in();
        EnteredSpan { span: self, here }
    }

    /// Enters it on this thread until the guard drops, keeping the handle.
    #[inline(always)]
    pub fn enter(&self) -> Entered<'_> {
        let here = self.edge_in();
        Entered { span: self, here }
    }

    /// Runs `f` inside it.
    pub fn in_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.enter();
        f()
    }

    /// The span id of a ring span, writing its open entry if it has none yet.
    fn ring_id(site: &'static Site, span: &AtomicU64) -> u64 {
        let id = span.load(Ordering::Relaxed);
        if id != 0 {
            return id;
        }
        let Some(config) = record::config() else { return 0 };
        let id = record::open(config, site.id(), Parent::Current, &[]);
        span.store(id, Ordering::Relaxed);
        id
    }

    /// Gives its field `field` the value `value`. Nothing for a field its site did not name,
    /// or a value that is `None`.
    pub fn record<V: ToValue>(&self, field: &str, value: V) -> &Span {
        match &self.inner {
            Inner::None => {}
            Inner::Ring { site, config, span } => {
                if let Some(field) = site.metadata().fields().field(field)
                    && let Some(value) = value.to_value()
                {
                    let id = Span::ring_id(*site, span);
                    record::record(config, site.id(), id, field.index() as u16, &value);
                }
            }
            Inner::Sink(open) => {
                if let Some(field) = open.meta.fields().field(field)
                    && let Some(value) = value.to_value()
                {
                    open.sink.record(&open.id, &Record { field, value });
                }
            }
        }
        self
    }

    /// Its id, `None` when nothing records it. A ring span's id is its first entry's.
    pub fn id(&self) -> Option<Id> {
        match &self.inner {
            Inner::None => None,
            Inner::Ring { site, span, .. } => match Span::ring_id(*site, span) {
                0 => None,
                id => Some(Id(id)),
            },
            Inner::Sink(open) => Some(open.id),
        }
    }

    #[inline(always)]
    pub fn is_none(&self) -> bool {
        matches!(self.inner, Inner::None)
    }

    pub fn is_disabled(&self) -> bool {
        self.is_none()
    }

    pub fn metadata(&self) -> Option<&'static Metadata<'static>> {
        match &self.inner {
            Inner::None => None,
            Inner::Ring { site, .. } => Some(site.metadata()),
            Inner::Sink(open) => Some(open.meta),
        }
    }
}

impl Clone for Span {
    fn clone(&self) -> Span {
        Span {
            inner: match &self.inner {
                Inner::None => Inner::None,
                Inner::Ring { site, config, span } => {
                    Inner::Ring { site: *site, config: *config, span: AtomicU64::new(span.load(Ordering::Relaxed)) }
                }
                Inner::Sink(open) => Inner::Sink(Open { id: open.sink.clone_span(&open.id), ..*open }),
            },
        }
    }
}

impl Drop for Span {
    /// Nothing for a ring span or none (inlined into every site); a sink's close is out of line.
    #[inline(always)]
    fn drop(&mut self) {
        #[cold]
        #[inline(never)]
        fn close(open: &Open) {
            open.sink.try_close(open.id);
        }
        if let Inner::Sink(open) = &self.inner {
            close(open);
        }
    }
}

impl Default for Span {
    #[inline(always)]
    fn default() -> Span {
        Span::none()
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::None => f.write_str("Span(none)"),
            Inner::Ring { site, span, .. } => f
                .debug_struct("Span")
                .field("name", &site.metadata().name())
                .field("id", &span.load(Ordering::Relaxed))
                .finish(),
            Inner::Sink(open) => f.debug_struct("Span").field("name", &open.meta.name()).field("id", &open.id).finish(),
        }
    }
}

/// A span entered on this thread, owning its handle: left when it drops. Not sent to another
/// thread, where it was never entered.
pub struct EnteredSpan {
    span: Span,
    /// The entering thread's state ([`Span::edge_in`]); a raw pointer, so the guard is not
    /// `Send`.
    here: *const record::Thread,
}

impl EnteredSpan {
    /// Leaves it, keeping it open.
    pub fn exit(mut self) -> Span {
        let span = core::mem::replace(&mut self.span, Span::none());
        let here = core::mem::replace(&mut self.here, core::ptr::null());
        span.edge_out(here);
        span
    }

    pub fn id(&self) -> Option<Id> {
        self.span.id()
    }
}

impl core::ops::Deref for EnteredSpan {
    type Target = Span;

    fn deref(&self) -> &Span {
        &self.span
    }
}

impl Drop for EnteredSpan {
    #[inline(always)]
    fn drop(&mut self) {
        if !self.here.is_null() {
            self.span.leave(self.here);
        }
    }
}

impl fmt::Debug for EnteredSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.span.fmt(f)
    }
}

/// A span entered on this thread through a borrow ([`Span::enter`]): left when it drops.
pub struct Entered<'a> {
    span: &'a Span,
    /// As an `EnteredSpan`'s: the entering thread's state.
    here: *const record::Thread,
}

impl Drop for Entered<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        if !self.here.is_null() {
            self.span.leave(self.here);
        }
    }
}
