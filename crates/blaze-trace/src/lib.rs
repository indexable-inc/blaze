//! blaze's spans and events: the one home of tracing in blaze (docs/v2/plan/design/no-tracing.md,
//! docs/v2/plan/third-party.md's `tracing` row). The `tracing` and `tracing-subscriber` crates
//! are gone; every crate writes these macros and the flight recorder (blaze-profile's
//! `Recorder`) is the sink that keeps what they say.
//!
//! - A site is static data, made by the macro where it is written: its name, target, level,
//!   file, line and field names ([`Metadata`]). Nothing about a site is registered here; a
//!   sink keys whatever it keeps by the site's address.
//! - A span or an event is plain data handed to the sink as it happens: its site, its parent
//!   ([`Parent`]) and its fields' values ([`Value`]: numbers as themselves, text borrowed, a
//!   `Display` or `Debug` value borrowed and formatted only if the sink wants text). A sink that
//!   keeps time reads its own clock; [`capture::Capture`] takes one passed in.
//! - The macros have the call shape crates already wrote (`info_span!(target: "blaze", "name",
//!   krate = %k, unit)`, `warn!(%dir, "text {x}")`, `field::Empty` then `span.record(..)`), so
//!   moving a crate here changes its import and nothing else.
//!
//! # The one ambient hook
//!
//! Rule 75 (nothing ambient) says the clock, the environment and IO are passed in. A span site
//! has no handle in scope (it is a line in a parser or a type checker, 300 of them), so its sink
//! is reached ambiently, through exactly one hook: the sink of the calling thread
//! ([`set_thread`]), or else the one the program's entry installed ([`install`], once a
//! process, never replaced). That is the whole of it: no dispatcher stack, no registry, no
//! callsite interest cache. A thread a pool or a test spawns reaches the entry's sink with no
//! step of its own, which is why the entry's sink is a process value and not only a thread's:
//! the flight recorder must see every thread's spans. A task that wants its own record (a
//! test, a simulation) sets its thread's sink and gets the entry's back when it is done
//! ([`with_thread_sink`]). Code that has a context (`bstd::Cx`) records through it instead.
//!
//! # What a site costs with no sink
//!
//! One thread-local read, one relaxed atomic load and a branch: the macro evaluates no field
//! and formats nothing until a sink said it wants the site ([`enabled`]). `trace!` and
//! `trace_span!` compile out of builds without debug assertions ([`Level::COMPILED`]).
//! Evidence: [`tests::no_sink_evaluates_no_field`].

#![no_std]
#![feature(thread_local)]

extern crate alloc;

use core::{
    cell::{Cell, UnsafeCell},
    fmt,
    marker::PhantomData,
    sync::atomic::{AtomicU8, Ordering},
};

pub mod capture;
mod macros;
mod value;

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
    /// Below every level: what a sink that keeps nothing says ([`Sink::max_level`]).
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
type Values<'a> = &'a [Option<Value<'a>>];

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

    /// Gives the event to `sink`: what each event macro does once [`enabled`] said yes.
    pub fn dispatch(sink: &dyn Sink, meta: &'static Metadata<'static>, parent: Parent, values: Values<'_>) {
        sink.event(&Event { meta, values, parent });
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
    /// The most verbose level it keeps anything of: installing it raises the process's group
    /// flag ([`wanted`]) to it, so a site above every sink's level costs one relaxed load.
    fn max_level(&self) -> Level {
        Level::TRACE
    }

    /// Whether it keeps `meta`'s spans or events: asked at every site reached, before any field
    /// is evaluated.
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
    fn max_level(&self) -> Level {
        Level::NONE
    }

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

/// The calling thread's own sink, ahead of the entry's.
#[thread_local]
static THREAD: Cell<Option<&'static dyn Sink>> = Cell::new(None);

/// The entry's sink: written once under [`ENTRY_STATE`], then only read.
struct EntrySink(UnsafeCell<Option<&'static dyn Sink>>);

// SAFETY: written once, by the one `install` that moved ENTRY_STATE from EMPTY to WRITING, and
// read only after ENTRY_STATE is READY (release, then acquire), so no read races the write.
unsafe impl Sync for EntrySink {}

static ENTRY: EntrySink = EntrySink(UnsafeCell::new(None));
static ENTRY_STATE: AtomicU8 = AtomicU8::new(EMPTY);
const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

/// The group flag: the most verbose level any sink installed in this process keeps (0 while
/// none is). Raised as a sink is installed and never lowered, so it only says "maybe": a site
/// more verbose than it stops at one relaxed load, before the thread's sink is read.
static MOST: AtomicU8 = AtomicU8::new(0);

fn raise(sink: &dyn Sink) {
    MOST.fetch_max(sink.max_level().0, Ordering::Relaxed);
}

/// Whether any sink of the process may keep a site at `level`: the first thing every macro
/// asks after [`Level::compiled`], one relaxed load of a `static` and a branch.
#[inline(always)]
pub fn wanted(level: Level) -> bool {
    level.0 <= MOST.load(Ordering::Relaxed)
}

/// The entry's sink was installed already: a process has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installed;

/// Makes `sink` the process's: every thread with no sink of its own records into it. The
/// program's entry calls it once, before it starts work; a second call is refused and changes
/// nothing.
pub fn install(sink: &'static dyn Sink) -> Result<(), Installed> {
    if ENTRY_STATE.compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return Err(Installed);
    }
    // SAFETY: this call alone moved the state to WRITING, and no reader reads before READY.
    unsafe { *ENTRY.0.get() = Some(sink) };
    ENTRY_STATE.store(READY, Ordering::Release);
    raise(sink);
    Ok(())
}

#[inline]
fn entry() -> Option<&'static dyn Sink> {
    if ENTRY_STATE.load(Ordering::Acquire) != READY {
        return None;
    }
    // SAFETY: READY was stored after the one write, with release, and read here with acquire.
    unsafe { *ENTRY.0.get() }
}

/// Sets the calling thread's own sink (`None`: the entry's again): the one it had.
pub fn set_thread(sink: Option<&'static dyn Sink>) -> Option<&'static dyn Sink> {
    if let Some(sink) = sink {
        raise(sink);
    }
    THREAD.replace(sink)
}

/// Runs `f` with `sink` as the calling thread's, then gives the thread back the one it had,
/// also when `f` panics.
pub fn with_thread_sink<R>(sink: &'static dyn Sink, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static dyn Sink>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THREAD.set(self.0);
        }
    }
    let _restore = Restore(set_thread(Some(sink)));
    f()
}

/// The sink the calling thread records into: its own, else the entry's.
#[inline]
pub fn current() -> Option<&'static dyn Sink> {
    match THREAD.get() {
        Some(sink) => Some(sink),
        None => entry(),
    }
}

/// The sink to give `meta`'s span or event to, if any wants it: what every macro asks before
/// it evaluates a field.
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

/// A handle of an open span: it closes when its last handle drops. Made by the span macros;
/// none when no sink wanted it, and then every call on it does nothing.
pub struct Span {
    open: Option<Open>,
}

impl Span {
    /// The span no sink wanted.
    pub const fn none() -> Span {
        Span { open: None }
    }

    /// Begins a span of `meta` in `sink`: what the span macros do once [`enabled`] said yes.
    pub fn new(sink: &'static dyn Sink, meta: &'static Metadata<'static>, parent: Parent, values: Values<'_>) -> Span {
        let id = sink.new_span(&Attributes { meta, values, parent });
        Span { open: Some(Open { sink, id, meta }) }
    }

    /// Enters it on this thread until the guard drops.
    pub fn entered(self) -> EnteredSpan {
        if let Some(open) = &self.open {
            open.sink.enter(&open.id);
        }
        EnteredSpan { span: self, _here: PhantomData }
    }

    /// Enters it on this thread until the guard drops, keeping the handle.
    pub fn enter(&self) -> Entered<'_> {
        if let Some(open) = &self.open {
            open.sink.enter(&open.id);
        }
        Entered { span: self, _here: PhantomData }
    }

    /// Runs `f` inside it.
    pub fn in_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.enter();
        f()
    }

    /// Gives its field `field` the value `value`. Nothing for a field its site did not name,
    /// or a value that is `None`.
    pub fn record<V: ToValue>(&self, field: &str, value: V) -> &Span {
        if let Some(open) = &self.open
            && let Some(field) = open.meta.fields().field(field)
            && let Some(value) = value.to_value()
        {
            open.sink.record(&open.id, &Record { field, value });
        }
        self
    }

    /// Its id, `None` when no sink wanted it.
    pub fn id(&self) -> Option<Id> {
        self.open.map(|it| it.id)
    }

    pub fn is_none(&self) -> bool {
        self.open.is_none()
    }

    pub fn is_disabled(&self) -> bool {
        self.open.is_none()
    }

    pub fn metadata(&self) -> Option<&'static Metadata<'static>> {
        self.open.map(|it| it.meta)
    }
}

impl Clone for Span {
    fn clone(&self) -> Span {
        Span {
            open: self.open.map(|open| Open { id: open.sink.clone_span(&open.id), ..open }),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(open) = self.open.take() {
            open.sink.try_close(open.id);
        }
    }
}

impl Default for Span {
    fn default() -> Span {
        Span::none()
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.open {
            Some(open) => f.debug_struct("Span").field("name", &open.meta.name()).field("id", &open.id).finish(),
            None => f.write_str("Span(none)"),
        }
    }
}

/// A span entered on this thread, owning its handle: left when it drops. Not sent to another
/// thread, where it was never entered.
pub struct EnteredSpan {
    span: Span,
    _here: PhantomData<*mut ()>,
}

impl EnteredSpan {
    /// Leaves it, keeping it open.
    pub fn exit(mut self) -> Span {
        let span = core::mem::replace(&mut self.span, Span::none());
        if let Some(open) = &span.open {
            open.sink.exit(&open.id);
        }
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
    fn drop(&mut self) {
        if let Some(open) = &self.span.open {
            open.sink.exit(&open.id);
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
    _here: PhantomData<*mut ()>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        if let Some(open) = &self.span.open {
            open.sink.exit(&open.id);
        }
    }
}
