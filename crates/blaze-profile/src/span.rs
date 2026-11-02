//! Typed spans: each span whose columns the profile reads (a unit's phases, a macro's expansion,
//! a wait, a hold, a job of the pool) is a kind, a struct of typed fields, declared once in the
//! [`span_kinds!`] registry at the end of this file. That one declaration makes:
//! - the `spans` table's columns the fields fill ([`GENERIC_COLUMNS`] and [`TYPED_COLUMNS`],
//!   which [`crate::schema::SPANS`] is made of, and which `batches.rs` writes from [`CELLS`]);
//! - each kind's encoder into its flight-recorder slot ([`Fields::write`]): a word a field,
//!   with no `tracing` field visitor and no field name matched as text;
//! - its readers: of a ring's row ([`read`]), of a `spans` batch ([`read_batch`]), and a SQL
//!   view of its rows with its fields as columns ([`views`]: `span_macro_expand`).
//!
//! A field names the column it fills by a type (`=> Steps` is `col::Steps`), so a misspelled
//! column is a compile error; a kind is a struct literal, so a site that leaves out a field does
//! not compile; a field whose type is not its column's does not compile ([`FieldOf`]); and a
//! typed column no kind writes fails the build (`WRITTEN`), so no column is null by construction.
//!
//! Ad hoc `tracing::info_span!(target: "blaze", ..)` spans keep working: they fill the generic
//! columns (`crate`, `function`, `unit`) from fields of those names, as before. The typed
//! columns (`awaits`, `owner`, `steps`, ...) only a kind writes.

use std::{
    cell::RefCell,
    fmt,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use arrow_array::{
    Array, RecordBatch,
    cast::AsArray,
    types::{UInt32Type, UInt64Type},
};
use blaze_graph::wait::WaitKind;
use blaze_hash::HashMap;

use crate::{
    flight::{self, SpanRow, Sym},
    schema::{self, Type},
};

// --- Values -----------------------------------------------------------------------------

/// A name a field holds (a crate, a macro's path, a unit, a key), interned: a [`Sym`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Name(Sym);

impl Name {
    /// `text`'s name: a hash and one lock of a shard of the recorder's names ([`flight::intern`]).
    pub fn of(text: &str) -> Name {
        Name(flight::intern(text))
    }

    pub fn sym(self) -> Sym {
        self.0
    }

    /// Its text: none once its generation of names was let go.
    pub fn text(self) -> Option<Arc<str>> {
        flight::name(self.0)
    }
}

thread_local! {
    /// The names [`Name::kept`] gave on this thread, by the address and length of their text.
    // alloc: once per thread, grown to the kept names it opens spans of
    static KEPT: RefCell<HashMap<(usize, usize), Name>> = RefCell::new(HashMap::default());
}

impl Name {
    /// `text`'s name for the life of the process, found by the address of `text` after its
    /// first time on a thread: for the few names code holds as `&'static str`, a job's kind.
    #[inline]
    pub fn kept(text: &'static str) -> Name {
        let key = (text.as_ptr().addr(), text.len());
        if let Some(name) = KEPT.try_with(|kept| kept.borrow().get(&key).copied()).ok().flatten() {
            return name;
        }
        Name::keep(key, text)
    }

    #[cold]
    fn keep(key: (usize, usize), text: &'static str) -> Name {
        let name = Name(flight::intern_kept(text));
        _ = KEPT.try_with(|kept| kept.borrow_mut().insert(key, name));
        name
    }
}

/// How a run ended, as a `macro expand`'s column `outcome` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The macro gave its output.
    Ok,
    /// The macro panicked.
    Panic,
    StepLimit,
    MemoryLimit,
    /// The evaluator, a shim or the server stopped it for another reason.
    Error,
}

impl Outcome {
    /// Every outcome, each at its [`Outcome::index`].
    pub const ALL: [Outcome; 5] = [Outcome::Ok, Outcome::Panic, Outcome::StepLimit, Outcome::MemoryLimit, Outcome::Error];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Panic => "panic",
            Outcome::StepLimit => "step limit",
            Outcome::MemoryLimit => "memory limit",
            Outcome::Error => "error",
        }
    }
}

/// A value a column holds, as one word of a span's slot and ring rows: never 0, which is "not
/// said".
pub trait Value: Copy + fmt::Debug + PartialEq {
    /// Whether its column holds text (`Dict`, `Utf8`) rather than a number (`U64`).
    const TEXT: bool;
    fn word(self) -> u64;
    /// The value `word` is: `None` for 0, or for a word no value of it writes.
    fn of_word(word: u64) -> Option<Self>;
    /// The cell of a text column for `word`.
    fn text(word: u64) -> Option<Arc<str>>;
    /// The cell of a number column for `word`.
    fn number(word: u64) -> Option<u64>;
    /// The word of a text cell read back from a table (0: none of these is named so).
    fn of_text(text: &str) -> u64;
}

impl Value for Name {
    const TEXT: bool = true;

    #[inline]
    fn word(self) -> u64 {
        u64::from(self.0)
    }

    fn of_word(word: u64) -> Option<Name> {
        Sym::try_from(word).ok().filter(|it| *it != 0).map(Name)
    }

    fn text(word: u64) -> Option<Arc<str>> {
        flight::name(Sym::try_from(word).ok()?)
    }

    fn number(_: u64) -> Option<u64> {
        None
    }

    fn of_text(text: &str) -> u64 {
        Name::of(text).word()
    }
}

impl Value for u64 {
    const TEXT: bool = false;

    /// Plus one, saturating: `u64::MAX - 1` and `u64::MAX` read back as `u64::MAX - 1`.
    #[inline]
    fn word(self) -> u64 {
        self.saturating_add(1)
    }

    fn of_word(word: u64) -> Option<u64> {
        word.checked_sub(1)
    }

    fn text(_: u64) -> Option<Arc<str>> {
        None
    }

    fn number(word: u64) -> Option<u64> {
        word.checked_sub(1)
    }

    fn of_text(_: &str) -> u64 {
        0
    }
}

/// A field that may go unsaid: `None` is the word 0.
impl<T: Value> Value for Option<T> {
    const TEXT: bool = T::TEXT;

    #[inline]
    fn word(self) -> u64 {
        self.map_or(0, T::word)
    }

    fn of_word(word: u64) -> Option<Option<T>> {
        Some(T::of_word(word))
    }

    fn text(word: u64) -> Option<Arc<str>> {
        T::text(word)
    }

    fn number(word: u64) -> Option<u64> {
        T::number(word)
    }

    fn of_text(text: &str) -> u64 {
        T::of_text(text)
    }
}

/// An enum with `ALL`, `index` and `name` as a [`Value`]: its index plus one.
macro_rules! enum_values {
    ($($ty:ty),* $(,)?) => {$(
        impl Value for $ty {
            const TEXT: bool = true;

            #[inline]
            fn word(self) -> u64 {
                self.index() as u64 + 1
            }

            fn of_word(word: u64) -> Option<$ty> {
                <$ty>::ALL.get(usize::try_from(word.checked_sub(1)?).ok()?).copied()
            }

            fn text(word: u64) -> Option<Arc<str>> {
                // alloc: once per distinct value a column of a batch
                Self::of_word(word).map(|it| Arc::from(it.name()))
            }

            fn number(_: u64) -> Option<u64> {
                None
            }

            fn of_text(text: &str) -> u64 {
                <$ty>::ALL.iter().find(|it| it.name() == text).map_or(0, |it| it.word())
            }
        }
    )*};
}

enum_values!(Outcome, WaitKind);

/// The fields a column of value `V` may have: `V`, or `Option<V>` where it may go unsaid.
pub trait FieldOf<V: Value>: Value {}

macro_rules! fields_of {
    ($($ty:ty),* $(,)?) => {$(
        impl FieldOf<$ty> for $ty {}
        impl FieldOf<$ty> for Option<$ty> {}
    )*};
}

fields_of!(Name, u64, Outcome, WaitKind);

// --- Columns ----------------------------------------------------------------------------

/// A column of `spans` that span kinds fill: where its word is in [`Said`], what it holds, and
/// its place in the schema.
pub trait Column {
    /// Its index in [`Said::get`]: the generic columns first.
    const AT: usize;
    type Value: Value;
    const SCHEMA: schema::Column;
}

/// The words of a span's columns, as its slot holds them: the generic ones (names, 0 for none)
/// and the typed ones (0: not said).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Said {
    pub krate: Sym,
    pub function: Sym,
    pub unit: Sym,
    pub typed: [u64; TYPED],
}

const CRATE: usize = <col::Crate as Column>::AT;
const FUNCTION: usize = <col::Function as Column>::AT;
const UNIT: usize = <col::Unit as Column>::AT;

impl Said {
    pub const EMPTY: Said = Said { krate: 0, function: 0, unit: 0, typed: [0; TYPED] };

    /// The word of the column at `at` ([`Column::AT`]).
    #[inline]
    pub fn get(&self, at: usize) -> u64 {
        match at {
            CRATE => u64::from(self.krate),
            FUNCTION => u64::from(self.function),
            UNIT => u64::from(self.unit),
            _ => at.checked_sub(GENERIC).and_then(|it| self.typed.get(it)).copied().unwrap_or(0),
        }
    }

    /// Sets the column at `at` to `word`; a generic column's word is a [`Sym`].
    #[inline]
    pub fn set(&mut self, at: usize, word: u64) {
        let sym = || Sym::try_from(word).unwrap_or(0);
        match at {
            CRATE => self.krate = sym(),
            FUNCTION => self.function = sym(),
            UNIT => self.unit = sym(),
            _ => {
                if let Some(it) = at.checked_sub(GENERIC).and_then(|it| self.typed.get_mut(it)) {
                    *it = word;
                }
            }
        }
    }

    /// Column `C`'s value: `None` when not said.
    pub fn value<C: Column>(&self) -> Option<C::Value> {
        C::Value::of_word(self.get(C::AT))
    }
}

/// How `batches.rs` writes a column and [`read_batch`] reads it back, made from its declaration.
#[derive(Clone, Copy)]
pub struct Cells {
    pub column: schema::Column,
    pub at: usize,
    pub text: fn(u64) -> Option<Arc<str>>,
    pub number: fn(u64) -> Option<u64>,
    pub of_text: fn(&str) -> u64,
}

impl Cells {
    const fn of<C: Column>() -> Cells {
        Cells {
            column: C::SCHEMA,
            at: C::AT,
            text: <C::Value as Value>::text,
            number: <C::Value as Value>::number,
            of_text: <C::Value as Value>::of_text,
        }
    }
}

#[inline]
fn put<C: Column, F: FieldOf<C::Value>>(said: &mut Said, value: F) {
    said.set(C::AT, value.word());
}

#[inline]
fn take<C: Column, F: FieldOf<C::Value>>(said: &Said) -> Option<F> {
    F::of_word(said.get(C::AT))
}

/// Holds when a column of `ty` holds what `text` says its values are, and `ty` is a type a
/// column of span fields may be.
const fn holds(ty: Type, text: bool) -> bool {
    match ty {
        Type::U64 => !text,
        Type::Utf8 | Type::Dict => text,
        _ => false,
    }
}

// --- Kinds ------------------------------------------------------------------------------

/// The fields of a kind, or of what it says as it ends: written into a span's [`Said`] and read
/// back from it.
pub trait Fields: Copy + fmt::Debug + PartialEq {
    fn write(self, said: &mut Said);
    /// `None` when a field that is always said is not.
    fn read(said: &Said) -> Option<Self>;
}

/// A kind that says nothing as it ends.
impl Fields for () {
    fn write(self, _: &mut Said) {}

    fn read(_: &Said) -> Option<()> {
        Some(())
    }
}

/// A span kind: its name (the `spans.name` of its rows) and the fields it ends with. A site
/// that leaves out a field does not compile (not run: the crate has `doctest = false`):
///
/// ```compile_fail,E0063
/// use blaze_profile::span::{MacroExpand, Name};
/// let _span = MacroExpand { path: Name::of("serde_derive::Serialize") }.open();
/// ```
pub trait Span: Fields {
    const NAME: &'static str;
    /// What it says as it ends, with [`Ending::ended`]; `()` for a kind that says it all as it
    /// begins.
    type Ended: Fields;
    /// Its site in the flight recorder: `None` past the recorder's table of sites.
    fn site() -> Option<u16>;
}

/// The callsite of a kind: what its `tracing` metadata names, so a kind's site is one of the
/// recorder's as any span's is ([`flight::register`]). No subscriber ever sees it.
pub struct KindCallsite(pub &'static tracing::Metadata<'static>);

impl tracing::callsite::Callsite for KindCallsite {
    fn set_interest(&self, _: tracing::subscriber::Interest) {}

    fn metadata(&self) -> &tracing::Metadata<'_> {
        self.0
    }
}

/// The metadata of the kind `name`: target `blaze`, info, no fields.
pub const fn kind_meta(name: &'static str, callsite: &'static KindCallsite) -> tracing::Metadata<'static> {
    tracing::Metadata::new(
        name,
        "blaze",
        tracing::Level::INFO,
        Some(file!()),
        Some(line!()),
        Some(module_path!()),
        tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(callsite)),
        tracing::metadata::Kind::SPAN,
    )
}

/// A kind's site, registered as its first span opens: 0 until then, `REFUSED` past the table.
pub struct SiteOnce(AtomicU32);

const REFUSED: u32 = u32::MAX;

impl SiteOnce {
    pub const fn new() -> SiteOnce {
        SiteOnce(AtomicU32::new(0))
    }

    #[inline]
    pub fn get(&self, meta: &'static tracing::Metadata<'static>) -> Option<u16> {
        match self.0.load(Ordering::Relaxed) {
            0 => self.register(meta),
            REFUSED => None,
            site => u16::try_from(site).ok(),
        }
    }

    #[cold]
    fn register(&self, meta: &'static tracing::Metadata<'static>) -> Option<u16> {
        let site = flight::register(meta);
        self.0.store(site.map_or(REFUSED, u32::from), Ordering::Relaxed);
        site
    }
}

/// An open span of a kind, entered on this thread: it leaves and closes when dropped, on the
/// thread it is dropped on (a job's fiber may have moved).
#[must_use = "a span closes when its guard drops"]
pub struct Entered {
    id: u64,
    /// Not sent to another thread: it is entered on this one's stack of spans.
    _here: PhantomData<*const ()>,
}

impl Entered {
    fn of<K: Span>(kind: K) -> Entered {
        Entered { id: open_id(kind), _here: PhantomData }
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        close_id(self.id);
    }
}

/// An open span of a kind that says more as it ends: [`Ending::ended`] says it and closes the
/// span. Dropped without it (an unwind, an early return), the span closes with those columns
/// null, which reads as "did not end".
#[must_use = "a span that ends with fields says them with `ended`"]
pub struct Ending<E: Fields> {
    entered: Entered,
    _ended: PhantomData<E>,
}

impl<E: Fields> Ending<E> {
    fn of<K: Span<Ended = E>>(kind: K) -> Ending<E> {
        Ending { entered: Entered::of(kind), _ended: PhantomData }
    }

    /// Says `ended`, then closes the span.
    pub fn ended(self, ended: E) {
        flight::said_after(self.entered.id, |said| ended.write(said));
    }
}

/// Opens `kind` on this thread and enters it: its id, [`flight::UNRECORDED`] when not recorded.
#[inline]
fn open_id<K: Span>(kind: K) -> u64 {
    let Some(site) = K::site() else { return flight::UNRECORDED };
    let mut said = Said::EMPTY;
    kind.write(&mut said);
    let id = flight::open_said(site, &said);
    crate::profile_began(id);
    id
}

#[inline]
fn close_id(id: u64) {
    flight::exited(id);
    crate::closed(id);
}

/// Opens and enters `kind` with no guard: its id for [`close_raw`], which a wait's end calls on
/// whatever thread it ended on.
pub(crate) fn open_raw<K: Span>(kind: K) -> u64 {
    open_id(kind)
}

pub(crate) fn close_raw(id: u64) {
    close_id(id);
}

// --- Reading back -----------------------------------------------------------------------

/// A span of kind `K` read back: its fields, and what it said as it ended (`None`: it did not).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Typed<K: Span> {
    pub fields: K,
    pub ended: Option<K::Ended>,
}

/// Why a span of a kind could not be read back.
#[derive(Debug, PartialEq, Eq)]
pub enum KindError {
    /// A row of the kind lacks a field its kind always says.
    Unsaid { kind: &'static str, span_id: u64 },
    /// A batch lacks a column of the kind's table, or holds it as another type: a `spans`
    /// file of another layout than [`schema::VERSION`]'s.
    Column { kind: &'static str, column: &'static str, why: &'static str },
}

impl fmt::Display for KindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KindError::Unsaid { kind, span_id } => {
                write!(f, "the `{kind}` span {span_id} lacks a field every `{kind}` span says")
            }
            KindError::Column { kind, column, why } => write!(
                f,
                "reading `{kind}` spans: the spans column `{column}` {why} (spans schema {})",
                schema::VERSION
            ),
        }
    }
}

impl std::error::Error for KindError {}

/// `row` as a span of kind `K`: `None` when it is a span of another kind.
pub fn read<K: Span>(row: &SpanRow) -> Result<Option<Typed<K>>, KindError> {
    if K::site() != Some(row.site) {
        return Ok(None);
    }
    typed::<K>(&row.said(), row.span_id).map(Some)
}

fn typed<K: Span>(said: &Said, span_id: u64) -> Result<Typed<K>, KindError> {
    let fields = K::read(said).ok_or(KindError::Unsaid { kind: K::NAME, span_id })?;
    Ok(Typed { fields, ended: K::Ended::read(said) })
}

/// The spans of kind `K` in `batch`, a batch of the `spans` table as `batches.rs` writes it
/// and the Parquet history keeps it, in order. Names are interned in this process as they are
/// read.
pub fn read_batch<K: Span>(batch: &RecordBatch) -> Result<Vec<Typed<K>>, KindError> {
    let missing = |column: &'static str| KindError::Column { kind: K::NAME, column, why: "is not in the batch" };
    let names = batch.column_by_name("name").ok_or(missing("name"))?;
    let ids = batch.column_by_name("span_id").ok_or(missing("span_id"))?;
    let ids = ids.as_primitive_opt::<UInt64Type>().ok_or(KindError::Column {
        kind: K::NAME,
        column: "span_id",
        why: "is not UInt64",
    })?;
    // alloc: a read of a batch, once per column the kinds fill
    let mut columns = Vec::with_capacity(CELLS.len());
    for cells in &CELLS {
        let column = batch.column_by_name(cells.column.name).ok_or(missing(cells.column.name))?;
        columns.push((cells, column.as_ref()));
    }
    // alloc: the spans read, once a read
    let mut out = Vec::new();
    for row in 0..batch.num_rows() {
        if text_at(names.as_ref(), row) != Some(K::NAME) {
            continue;
        }
        let mut said = Said::EMPTY;
        for &(cells, column) in &columns {
            let word = if cells.column.ty == Type::U64 {
                let Some(numbers) = column.as_primitive_opt::<UInt64Type>() else {
                    return Err(KindError::Column { kind: K::NAME, column: cells.column.name, why: "is not UInt64" });
                };
                if numbers.is_valid(row) { numbers.value(row).saturating_add(1) } else { 0 }
            } else {
                text_at(column, row).map_or(0, cells.of_text)
            };
            said.set(cells.at, word);
        }
        out.push(typed::<K>(&said, ids.value(row))?);
    }
    Ok(out)
}

/// The text of a `Dict` or `Utf8` cell; `None` when null or neither.
fn text_at(column: &dyn Array, row: usize) -> Option<&str> {
    if !column.is_valid(row) {
        return None;
    }
    if let Some(dict) = column.as_dictionary_opt::<UInt32Type>() {
        let key = usize::try_from(dict.keys().value(row)).ok()?;
        let values = dict.values().as_string_opt::<i32>()?;
        return (key < values.len()).then(|| values.value(key));
    }
    column.as_string_opt::<i32>().map(|it| it.value(row))
}

/// A kind as its SQL view shows it: its name and each field with the column it fills.
pub struct KindTable {
    pub name: &'static str,
    pub fields: &'static [(&'static str, &'static str)],
}

/// The columns every kind's view has, before its fields.
const VIEW_COLUMNS: &str = "build_id, process, thread, thread_name, span_id, parent, depth, start_ns, dur_ns, self_cpu_ns";

/// Writes into `out` one view a kind, `span_<name>` (`span_macro_expand`): its rows of `spans`
/// with its fields as columns of their names (`path` for a `macro expand`'s `function`). What
/// [`crate::views::prelude`] ends with.
pub fn views(out: &mut String) {
    use fmt::Write as _;
    for kind in KINDS {
        // alloc: once per kind as a connection's views are made
        // Writing to a `String` does not fail.
        _ = write!(out, "\nCREATE OR REPLACE VIEW span_{} AS\nSELECT {VIEW_COLUMNS}", kind.name.replace(' ', "_"));
        for (field, column) in kind.fields {
            _ = write!(out, ", \"{column}\" AS \"{field}\"");
        }
        _ = write!(out, "\nFROM spans WHERE name = '{}';\n", kind.name);
    }
}

// --- The registry -----------------------------------------------------------------------

/// The one declaration of the columns span kinds fill and of the kinds. `generic` columns are
/// names that ad hoc `tracing` spans fill too, from the fields listed after `fields`; `typed`
/// columns only kinds fill. A kind lists `field: Type => Column`; one that says more as it
/// ends adds `=> Ended { .. }`, the fields [`Ending::ended`] takes.
macro_rules! span_kinds {
    (
        generic {
            $( $G:ident = $gname:literal ($gty:ident) fields $($galias:literal)|+, $gdoc:literal; )*
        }
        typed {
            $( $T:ident = $tname:literal ($tty:ident): $tval:ty, $tdoc:literal; )*
        }
        kinds {
            $(
                $(#[$kmeta:meta])*
                $K:ident = $kname:literal { $( $f:ident: $fty:ty => $fc:ident ),* $(,)? }
                $( => $E:ident { $( $ef:ident: $efty:ty => $efc:ident ),* $(,)? } )?
            )*
        }
    ) => {
        /// The columns' places in [`Said`].
        #[derive(Clone, Copy)]
        #[repr(usize)]
        enum Index { $( $G, )* $( $T, )* }

        /// The columns ad hoc spans fill too: names, at the first places of [`Said`].
        pub const GENERIC: usize = [$( stringify!($G) ),*].len();
        /// The columns only kinds fill.
        pub const TYPED: usize = [$( stringify!($T) ),*].len();
        pub const COLUMNS: usize = GENERIC + TYPED;

        /// A type a column: what a kind's field names after `=>`.
        pub mod col {
            $( pub struct $G; )*
            $( pub struct $T; )*
        }

        $(
            impl Column for col::$G {
                const AT: usize = Index::$G as usize;
                type Value = Name;
                const SCHEMA: schema::Column =
                    schema::Column { name: $gname, ty: Type::$gty, nullable: true, doc: $gdoc };
            }
        )*
        $(
            impl Column for col::$T {
                const AT: usize = Index::$T as usize;
                type Value = $tval;
                const SCHEMA: schema::Column =
                    schema::Column { name: $tname, ty: Type::$tty, nullable: true, doc: $tdoc };
            }
            const _: () = assert!(
                holds(Type::$tty, <$tval as Value>::TEXT),
                concat!("the column `", $tname, "` is U64 for a number, Utf8 or Dict for text")
            );
        )*
        $(
            const _: () = assert!(holds(Type::$gty, true), concat!("the column `", $gname, "` is Utf8 or Dict"));
        )*

        pub const GENERIC_COLUMNS: [schema::Column; GENERIC] = [$( <col::$G as Column>::SCHEMA ),*];
        pub const TYPED_COLUMNS: [schema::Column; TYPED] = [$( <col::$T as Column>::SCHEMA ),*];
        /// Every column, generic then typed, as `batches.rs` writes it.
        pub static CELLS: [Cells; COLUMNS] = [$( Cells::of::<col::$G>(), )* $( Cells::of::<col::$T>(), )*];

        /// The generic column a field of an ad hoc span named `field` fills.
        pub(crate) fn generic_of(field: &str) -> Option<usize> {
            match field {
                $( $($galias)|+ => Some(Index::$G as usize), )*
                _ => None,
            }
        }

        /// Which columns some kind's field fills.
        const WRITTEN: [bool; COLUMNS] = {
            let mut written = [false; COLUMNS];
            $(
                $( written[<col::$fc as Column>::AT] = true; )*
                $( $( written[<col::$efc as Column>::AT] = true; )* )?
            )*
            written
        };
        $(
            const _: () = assert!(
                WRITTEN[Index::$T as usize],
                concat!("no span kind writes the column `", $tname, "`: give a kind a field `=> ", stringify!($T), "` or remove it")
            );
        )*

        /// Every kind, as [`views`] shows it.
        pub const KINDS: &[KindTable] = &[$(
            KindTable {
                name: $kname,
                fields: &[
                    $( (stringify!($f), <col::$fc as Column>::SCHEMA.name), )*
                    $( $( (stringify!($ef), <col::$efc as Column>::SCHEMA.name), )* )?
                ],
            },
        )*];

        $(
            span_kinds!(@kind $(#[$kmeta])* $K = $kname { $( $f: $fty => $fc ),* } $( => $E { $( $ef: $efty => $efc ),* } )?);
        )*
    };

    (@fields $(#[$meta:meta])* $S:ident { $( $f:ident: $fty:ty => $fc:ident ),* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $S { $( pub $f: $fty, )* }

        impl Fields for $S {
            #[inline]
            fn write(self, said: &mut Said) {
                $( put::<col::$fc, $fty>(said, self.$f); )*
            }

            fn read(said: &Said) -> Option<$S> {
                Some($S { $( $f: take::<col::$fc, $fty>(said)?, )* })
            }
        }
    };

    (@site $kname:literal) => {
        fn site() -> Option<u16> {
            static CALLSITE: KindCallsite = KindCallsite(&META);
            static META: tracing::Metadata<'static> = kind_meta($kname, &CALLSITE);
            static SITE: SiteOnce = SiteOnce::new();
            SITE.get(&META)
        }
    };

    (@kind $(#[$meta:meta])* $K:ident = $kname:literal { $( $f:ident: $fty:ty => $fc:ident ),* }) => {
        span_kinds!(@fields $(#[$meta])* $K { $( $f: $fty => $fc ),* });

        impl Span for $K {
            const NAME: &'static str = $kname;
            type Ended = ();
            span_kinds!(@site $kname);
        }

        impl $K {
            /// Opens this span on this thread and enters it, until the guard drops.
            #[inline]
            pub fn open(self) -> Entered {
                Entered::of(self)
            }
        }
    };

    (@kind $(#[$meta:meta])* $K:ident = $kname:literal { $( $f:ident: $fty:ty => $fc:ident ),* }
        => $E:ident { $( $ef:ident: $efty:ty => $efc:ident ),* }) => {
        span_kinds!(@fields $(#[$meta])* $K { $( $f: $fty => $fc ),* });
        span_kinds!(@fields #[doc = concat!("What a `", $kname, "` says as it ends.")] $E { $( $ef: $efty => $efc ),* });

        impl Span for $K {
            const NAME: &'static str = $kname;
            type Ended = $E;
            span_kinds!(@site $kname);
        }

        impl $K {
            /// Opens this span on this thread and enters it, until [`Ending::ended`] or the
            /// guard drops.
            #[inline]
            pub fn open(self) -> Ending<$E> {
                Ending::of(self)
            }
        }
    };
}

span_kinds! {
    generic {
        Crate = "crate" (Dict) fields "crate" | "krate",
            "the crate it was of: a kind's `krate`, or an ad hoc span's field `crate`";
        Function = "function" (Utf8) fields "function" | "subject",
            "the function it was of: a `macro expand`'s macro path, what a `wait` awaits, what a `hold` holds, or an ad hoc span's field `function` or `subject`";
        Unit = "unit" (Utf8) fields "unit" | "cgu",
            "the unit it was of: a unit phase's package, version and target (`serde v1.0.219 (build script)`), or an ad hoc span's field `unit` (a codegen unit)";
    }
    typed {
        Awaits = "awaits" (Utf8): Name,
            "what a `wait` waits for: a query's key, a lock, a channel, a scope and the span its caller opened it in; every `wait` has it";
        Owner = "owner" (U64): u64,
            "who runs what a `wait` waits for (the job or thread running the awaited key; null when not known), or who holds a `hold`";
        Steps = "steps" (U64): u64,
            "the basic blocks a `macro expand` ran on the evaluator; null for a compiled run, a run that did not end, and any other span";
        PeakBytes = "peak_bytes" (U64): u64,
            "the most stack and heap a `macro expand`'s evaluator held at once; null as `steps` is";
        Outcome = "outcome" (Dict): Outcome,
            "how a `macro expand` ended: ok, panic, step limit, memory limit or error; null for one that did not end (an unwind)";
        WaitsOn = "waits_on" (Dict): WaitKind,
            "the kind of thing a `wait` waits on: key, lock, channel, io, child or scope";
    }
    kinds {
        /// A unit's names phase: its crate's names resolved, or its record replayed.
        UnitNames = "unit names" { krate: Name => Crate, unit: Name => Unit }
        /// A unit's bodies phase: the check of every body of its crate.
        UnitBodies = "unit bodies" { krate: Name => Crate, unit: Name => Unit }
        /// A unit's codegen phase: its functions compiled from its roots.
        UnitCodegen = "unit codegen" { krate: Name => Crate, unit: Name => Unit }
        /// A unit's link phase: its diagnostics shown and its image made.
        UnitLink = "unit link" { krate: Name => Crate, unit: Name => Unit }
        /// A unit's store phase: its outputs hashed into the store.
        UnitStore = "unit store" { krate: Name => Crate, unit: Name => Unit }
        /// One run of a proc macro: `path` the macro (`serde_derive::Serialize`), `krate` the
        /// crate the call is in.
        MacroExpand = "macro expand" { path: Name => Function, krate: Name => Crate }
            => MacroExpanded { steps: Option<u64> => Steps, peak_bytes: Option<u64> => PeakBytes, outcome: Outcome => Outcome }
        /// A thread or fiber blocked on `awaits` (also its `function`, which the sketches key
        /// by), run by `owner` when known.
        Wait = "wait" { awaits: Name => Awaits, function: Name => Function, owner: Option<u64> => Owner, on: WaitKind => WaitsOn }
        /// A hold of `what` by `holder`: what a waiter's `owner` names.
        Hold = "hold" { what: Name => Function, holder: u64 => Owner }
        /// A job a pool runs, `kind` the type of its closure (or of the plane's runner), from
        /// its start to its end or its fiber's park, and again from each resume: every job's
        /// time is inside one (`blaze_graph::pool::span_jobs`).
        Job = "job" { kind: Name => Function }
    }
}

/// The names of the unit phase spans, in the order a unit runs them.
pub const UNIT_PHASES: [&str; 5] =
    [UnitNames::NAME, UnitBodies::NAME, UnitCodegen::NAME, UnitLink::NAME, UnitStore::NAME];

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::batches::{self, Process};

    /// The spans of every kind, each with every field said, opened and closed on a thread of
    /// their own: that thread's id and the kinds' fields.
    fn spans_of_every_kind() -> (u32, MacroExpand, MacroExpanded, Wait, Hold) {
        crate::install();
        let krate = Name::of("span kinds test");
        let unit = Name::of("span-kinds v0.1.0 (lib)");
        let expand = MacroExpand { path: Name::of("span_kinds::Derive"), krate };
        let expanded = MacroExpanded { steps: Some(4_200_000), peak_bytes: Some(48 << 20), outcome: Outcome::MemoryLimit };
        let awaits = Name::of("span kinds key");
        let wait = Wait { awaits, function: awaits, owner: Some(7), on: WaitKind::Channel };
        let hold = Hold { what: Name::of("span kinds round"), holder: 9 };
        let tid = std::thread::spawn(move || {
            drop(UnitNames { krate, unit }.open());
            drop(UnitBodies { krate, unit }.open());
            drop(UnitCodegen { krate, unit }.open());
            drop(UnitLink { krate, unit }.open());
            drop(UnitStore { krate, unit }.open());
            expand.open().ended(expanded);
            drop(wait.open());
            drop(hold.open());
            crate::sampler::os_tid() as u32
        })
        .join()
        .unwrap();
        (tid, expand, expanded, wait, hold)
    }

    fn rows_of(tid: u32) -> Vec<SpanRow> {
        let mut read = flight::Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        read.spans.into_iter().filter(|it| it.thread == tid).collect()
    }

    #[test]
    fn every_kind_reads_back_with_every_field_from_the_ring_and_from_a_spans_batch() {
        let (tid, expand, expanded, wait, hold) = spans_of_every_kind();
        let rows = rows_of(tid);
        let from_ring = |row: &SpanRow| read::<MacroExpand>(row).unwrap();
        let expands: Vec<_> = rows.iter().filter_map(from_ring).collect();
        assert_eq!(expands, [Typed { fields: expand, ended: Some(expanded) }], "the ring's row is the kind");
        let waits: Vec<_> = rows.iter().filter_map(|it| read::<Wait>(it).unwrap()).collect();
        assert_eq!(waits, [Typed { fields: wait, ended: Some(()) }]);
        assert_eq!(rows.iter().filter_map(|it| read::<UnitNames>(it).unwrap()).count(), 1, "a kind is its own site");

        let batch = batches::spans(&rows, Process { pid: 1, name: "span kinds test", commit: None }).unwrap();
        assert_eq!(read_batch::<MacroExpand>(&batch).unwrap(), [Typed { fields: expand, ended: Some(expanded) }]);
        assert_eq!(read_batch::<Wait>(&batch).unwrap(), [Typed { fields: wait, ended: Some(()) }]);
        assert_eq!(read_batch::<Hold>(&batch).unwrap(), [Typed { fields: hold, ended: Some(()) }]);
        let phases = [
            read_batch::<UnitNames>(&batch).unwrap().len(),
            read_batch::<UnitBodies>(&batch).unwrap().len(),
            read_batch::<UnitCodegen>(&batch).unwrap().len(),
            read_batch::<UnitLink>(&batch).unwrap().len(),
            read_batch::<UnitStore>(&batch).unwrap().len(),
        ];
        assert_eq!(phases, [1; 5], "each phase is one span of its kind");
        let [names] = &read_batch::<UnitNames>(&batch).unwrap()[..] else { unreachable!() };
        assert_eq!(names.fields.unit.text().as_deref(), Some("span-kinds v0.1.0 (lib)"));
    }

    #[test]
    fn a_kind_dropped_before_it_ended_reads_as_not_ended() {
        crate::install();
        let path = Name::of("span_kinds::Unwound");
        let tid = std::thread::spawn(move || {
            drop(MacroExpand { path, krate: Name::of("span kinds unwound") }.open());
            crate::sampler::os_tid() as u32
        })
        .join()
        .unwrap();
        let rows = rows_of(tid);
        let [unwound] = &rows.iter().filter_map(|it| read::<MacroExpand>(it).unwrap()).collect::<Vec<_>>()[..] else {
            panic!("one `macro expand`")
        };
        assert_eq!(unwound.fields.path, path);
        assert_eq!(unwound.ended, None, "its `outcome` is null");
    }

    #[test]
    fn a_batch_of_another_layout_is_a_typed_error() {
        let (tid, ..) = spans_of_every_kind();
        let batch = batches::spans(&rows_of(tid), Process { pid: 1, name: "span kinds test", commit: None }).unwrap();
        let at = batch.schema().index_of("waits_on").unwrap();
        let mut older = batch.clone();
        older.remove_column(at);
        assert_eq!(
            read_batch::<Wait>(&older),
            Err(KindError::Column { kind: "wait", column: "waits_on", why: "is not in the batch" })
        );
    }

    #[test]
    fn every_typed_column_is_a_column_of_spans_after_the_generic_ones() {
        let spans = schema::SPANS;
        for cells in &CELLS {
            let at = spans.column(cells.column.name).unwrap_or_else(|| panic!("`{}` is a spans column", cells.column.name));
            assert_eq!(spans.columns[at].ty, cells.column.ty);
        }
        let last: Vec<&str> = spans.columns[spans.columns.len() - TYPED..].iter().map(|it| it.name).collect();
        assert_eq!(last, TYPED_COLUMNS.map(|it| it.name), "a typed column goes at the end, keeping `VERSION`");
        assert_eq!(spans.columns[schema::SPANS_GENERIC_AT].name, "crate");
    }

    #[test]
    fn each_kind_has_a_view_of_its_fields() {
        let mut sql = String::new();
        views(&mut sql);
        assert!(sql.contains("CREATE OR REPLACE VIEW span_macro_expand AS"), "{sql}");
        assert!(sql.contains("\"function\" AS \"path\""), "{sql}");
        assert!(sql.contains("\"outcome\" AS \"outcome\""), "{sql}");
        assert!(sql.contains("WHERE name = 'unit codegen'"), "{sql}");
    }
}
