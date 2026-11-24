//! D106 piece Y: the program FILLED ONE BODY AT A TIME, the end state of `blaze run`'s front
//! end (decision D106, docs/v2/plan/decisions.json; design and its legs in
//! blaze-int-1112-notes/port27/d106-run-on-records.md, "## Piece Y").
//!
//! [`super::Front2Program`] is made by a census: every fn `main` reaches is lowered, checked and
//! tabled before the back half asks anything (`make_from`), about 6 KB a row. [`LazyProgram`]
//! answers the same seam ([`crate::seam::FrontEnd`]) with NO census: its state is columns that
//! grow when a function is asked for, with one lowering per live body. The back half's
//! per-instance step (run/make_one.rs, piece X) drives it:
//!
//! 1. `check_one(key)` (`&mut self`, the ONE place a row is added): fn `key`'s row; its body
//!    lowered (each head and constant the lowering names made first: `lower_made`) and checked
//!    ONCE; then everything the build of THAT body asks the `&self` getters: a head row of each
//!    fn it calls, the row of each ADT its types name, the value of each constant it reads. The
//!    `Checked` is handed over owned (the program keeps none).
//! 2. the build of the instance reads the `&self` getters, which answer from the columns.
//! 3. The maker keeps the body for later instances at other arguments. A caller that releases
//!    it with `done(i)` drops its lowering and literals. The row's head facts stay.
//!
//! Why `&mut self` alone adds rows: the seam's getters lend `&'_` data (`FnFacts`, `FnSig`,
//! `Adt`, `PTys`) out of the program, so nothing may be added under `&self`; `fn_index` answers
//! only rows already made.
//!
//! THE NAMES SOURCE `S` (rule 78: one seam, each instantiation tested equal): `&Names` (the
//! names this command solved: what the equal-answers test compares with `Front2Program` over,
//! so only lazy-vs-whole differs) or `&source::Records` (each crate's names record opened on
//! the first question about that crate: the end state). Where a helper still reads only the
//! solved names (`NamesSource::solved`), the records instantiation refuses by name
//! ([`LazyRefusal::NeedsSolvedNames`]); the list is in the notes, "## Piece Y".
//!
//! Each live body owns its lowered types. Its view joins the prefix of the head table it saw
//! at lowering with its own nodes and lists, so later heads leave its indices valid.

mod fill;
mod front_end;
mod locate;
/// The read side of a body's typed record and its test double (D163 part 2).
pub mod typed;
/// The per-fn seam rules both programs answer by (rule 30), `Front2Program`'s calls included.
pub(in crate::front2::program) mod seam_rules;
pub use seam_rules::{PositionSource, body_place};

use super::*;

use blaze_hash::HashMap as Map;
use blaze_own::{AString, AVec};

use crate::front2::source::NamesSource;

/// Why the lazy program did not answer, naming the method or step and the item (rule 19).
#[derive(Debug)]
pub enum LazyRefusal {
    /// A key the program was asked about that its crate's records do not hold (lazy/locate.rs):
    /// no item, module or variant row, no member of its crate's containers, no item declared in
    /// a body the search read or lowered. `krate` names the crate its prefix is of; `searched`
    /// says what was read (rule 19).
    NotFound { key: ItemKey, krate: Box<str>, searched: Box<str> },
    /// `check_one` asked twice of one key: the step asks each key once (piece X's contract).
    AskedTwice { key: ItemKey },
    /// The body's lowering refused (`lower_made`).
    Lower { function: Box<str>, error: BodyError },
    /// The body's names could not be spelled.
    Spell { function: Box<str> },
    /// The body's check found a diagnostic, a gap or a refusal: never built.
    Check { function: Box<str>, error: CheckFail },
    /// The body's check met an item declared in a body whose form this front end does not
    /// make (a [`LocalGap`](crate::front2::locals::LocalGap)), printed as a user reads it
    /// (rule 19): the enclosing item's whole path, the local item's head as written and its
    /// `file:line` (rule 71: computed only here, from the item-relative range). `others`: the
    /// check's other gaps, counted.
    LocalNotBuilt { function: Box<str>, enclosing: Box<str>, form: &'static str, head: Box<str>, at: Box<str>, others: usize },
    /// The body's check found a diagnostic, printed as a user reads it (rule 19): its code, its
    /// words and its `file:line` (rule 71: computed only here, from the item-relative range).
    /// `others`: the check's other diagnostics, counted.
    Diagnostic { function: Box<str>, code: &'static str, about: Box<str>, at: Box<str>, others: usize },
    /// A row the build asks for (the fn's own, or a callee's head) is refused, as
    /// `program/tabled.rs` `row` refuses it.
    Row { function: Box<str>, refusal: Box<Front2Refusal> },
    /// A head (a fn's signature, its container's) could not be made.
    Head { item: Box<str>, why: Box<str> },
    /// An ADT the body's types name has no row.
    Adt { adt: Box<str>, why: Box<str> },
    /// A constant the body reads by value: the evaluator refused it.
    Const { constant: Box<str>, why: Box<str> },
    /// A step not made lazy yet: `method` of the seam would be asked about `item` by this
    /// body's build, and the lazy program has no per-body form of it yet (the notes, "## Piece
    /// Y", say what each needs). Refused at `check_one`, before the build asks.
    NotLazyYet { method: &'static str, item: Box<str> },
    /// The helper `helper` reads only the solved names (`&Names`), and this program's names are
    /// each crate's records: it needs its `NamesSource` form (the notes name each).
    NeedsSolvedNames { helper: &'static str, item: Box<str> },
    /// The names source did not answer `question` about `item` (over records the error is in
    /// the source's log, naming the crate and the record).
    Unanswered { question: &'static str, item: Box<str> },
    /// The types table did not fork.
    Table { function: Box<str> },
    /// The names records did not answer a question the make of the instance of fn `tabled`
    /// asked (or an earlier make of this session asked): what was made over them is void
    /// (source.rs module docs). `first` is the first error the records' log kept, as it names
    /// itself (the crate, the record, the item); `kept` how many the log kept, `dropped` how
    /// many past its bound it counted.
    RecordsVoid { tabled: ItemKey, first: Box<str>, kept: usize, dropped: u32 },
    /// Static `item`'s value or kind is missing from the program's sorted tables after it was
    /// made (fill.rs `static_value`: the two tables are filed together).
    NoStatic { item: ItemKey },
    /// A `format_args!` placeholder of `function`'s body needs core's `rt::Argument`
    /// constructor, and it was not found (fill.rs `fmt_rows`).
    FmtArgument { function: Box<str>, why: crate::front2::fmt_template::ArgumentNewFail },
    /// `function`'s checked columns were to be read from the check's record (typed.rs, the
    /// typing seam's `FromRecord`), and were not: never checked here instead (rule 3).
    Typed { function: Box<str>, gap: typed::TypedGap },
}

impl core::fmt::Display for LazyRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LazyRefusal::NotFound { key, krate, searched } => write!(f, "front2 (lazy): the key {:032x} of {krate} is no fn its records hold; searched {searched}", key.0),
            LazyRefusal::AskedTwice { key } => write!(f, "front2 (lazy): the body of the item with key {:032x} was asked for twice in one run", key.0),
            LazyRefusal::Lower { function, error } => write!(f, "front2 (lazy): `{function}`'s body did not lower: {error:?}"),
            LazyRefusal::Spell { function } => write!(f, "front2 (lazy): `{function}`'s names could not be spelled"),
            // Past the round limit: as said by path (check.rs `rounds_fail`), never the holes'
            // `Debug` (rt-loop a1750: 289 KB of keys on one line, no item named).
            LazyRefusal::Check { function, error: error @ CheckFail::Rounds { .. } } => write!(f, "front2 (lazy): `{function}` did not check clean: {error}"),
            LazyRefusal::Check { function, error } => write!(f, "front2 (lazy): `{function}` did not check clean: {error:?}"),
            LazyRefusal::LocalNotBuilt { function, enclosing, form, head, at, others } => {
                write!(f, "front2 (lazy): `{function}` did not check: in {enclosing}: `{head}` ({at}), {form}, is not built yet")?;
                match others {
                    0 => Ok(()),
                    n => write!(f, " (and {n} other gaps)"),
                }
            }
            LazyRefusal::Diagnostic { function, code, about, at, others } => {
                write!(f, "front2 (lazy): `{function}` did not check: error[{code}]: {about} ({at})")?;
                match others {
                    0 => Ok(()),
                    n => write!(f, " (and {n} other diagnostics)"),
                }
            }
            LazyRefusal::Row { refusal, .. } => write!(f, "front2 (lazy): {refusal}"),
            LazyRefusal::Head { item, why } => write!(f, "front2 (lazy): the head of `{item}`: {why}"),
            LazyRefusal::Adt { adt, why } => write!(f, "front2 (lazy): the ADT `{adt}`'s row: {why}"),
            LazyRefusal::Const { constant, why } => write!(f, "front2 (lazy): the constant `{constant}`: {why}"),
            LazyRefusal::NotLazyYet { method, item } => write!(f, "front2 (lazy): `{method}` about `{item}` has no per-body form yet"),
            LazyRefusal::NeedsSolvedNames { helper, item } => write!(f, "front2 (lazy): `{helper}` (asked about `{item}`) reads only solved names, not each crate's records"),
            LazyRefusal::Unanswered { question, item } => write!(f, "front2 (lazy): the names source did not answer `{question}` about `{item}` (its error is in the source's log)"),
            LazyRefusal::Table { function } => write!(f, "front2 (lazy): the types table did not fork for `{function}`"),
            LazyRefusal::RecordsVoid { tabled, first, kept, dropped } => write!(
                f,
                "front2 (lazy): the names records did not answer while the instance of fn {:032x} was made, so it is void: {first} ({kept} errors kept, {dropped} more counted)",
                tabled.0
            ),
            LazyRefusal::FmtArgument { function, why } => write!(f, "front2 (lazy): `{function}`: {why}"),
            LazyRefusal::Typed { function, gap } => write!(f, "front2 (lazy): `{function}`'s checked columns from the check's record: {gap}"),
            LazyRefusal::NoStatic { item } => write!(f, "front2 (lazy): the static {:032x} was made, and its value and kind are not both in the program's tables (fill.rs `static_value` files them together)", item.0),
        }
    }
}

/// A byte range of [`Rows::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    at: u32,
    len: u32,
}

/// The program's rows, one per fn asked about, as columns (struct of arrays; rule 11): a row
/// is its index in every column. Nothing here is a body: a row with a live body points into
/// [`LazyProgram::live`].
struct Rows<'n> {
    key: AVec<'n, ItemKey>,
    kind: AVec<'n, FnKind>,
    abi: AVec<'n, Abi>,
    /// Whether the fn is nounwind ([`crate::seam::FrontEnd::nounwind`]): its written ABI's
    /// unwind category (program/head_abi.rs `BuildAbi`, which [`Abi`] does not carry: "C" and
    /// "C-unwind" are one calling convention) or `#[rustc_nounwind]`, as tabled.rs `row`.
    nounwind: AVec<'n, bool>,
    /// The fn's OWN `#[track_caller]` (program.rs `symbols_of`); the seam's answer adds its
    /// trait's declaration's (program/seam_rules.rs `track_caller`).
    track_caller: AVec<'n, bool>,
    fn_attrs: AVec<'n, blaze_lir::func::FnAttrs>,
    at: AVec<'n, At>,
    path: AVec<'n, Span>,
    symbol: AVec<'n, Span>,
    export: AVec<'n, Option<Span>>,
    intrinsic: AVec<'n, Option<Intrinsic>>,
    /// The slot of [`LazyProgram::live`] holding the row's body; `None`: no body made, or done.
    live: AVec<'n, Option<u32>>,
    /// Whether `check_one` was asked of the row (each key once).
    checked: AVec<'n, bool>,
    /// Every row's path, symbol and export, end to end.
    text: AString<'n>,
    by_key: Map<ItemKey, u32, Alloc<'n>>,
}

impl<'n> Rows<'n> {
    fn new_in(alloc: Alloc<'n>) -> Rows<'n> {
        Rows {
            key: AVec::new_in(alloc),
            kind: AVec::new_in(alloc),
            abi: AVec::new_in(alloc),
            nounwind: AVec::new_in(alloc),
            track_caller: AVec::new_in(alloc),
            fn_attrs: AVec::new_in(alloc),
            at: AVec::new_in(alloc),
            path: AVec::new_in(alloc),
            symbol: AVec::new_in(alloc),
            export: AVec::new_in(alloc),
            intrinsic: AVec::new_in(alloc),
            live: AVec::new_in(alloc),
            checked: AVec::new_in(alloc),
            text: AString::new_in(alloc),
            by_key: Map::new_in(alloc),
        }
    }

    fn len(&self) -> usize {
        self.key.len()
    }

    fn put(&mut self, s: &str) -> Span {
        let at = self.text.len() as u32;
        self.text.push_str(s);
        Span { at, len: s.len() as u32 }
    }

    fn str(&self, s: Span) -> &str {
        &self.text[s.at as usize..(s.at + s.len) as usize]
    }
}

/// The checked lowering and literals lent to each instance of this row until `done`.
struct Live<'n> {
    row: u32,
    made: Body<'n>,
    lits: Literals,
}

/// What the lazy program did, for the counts a test and the run's line quote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LazyCounts {
    /// Bodies lowered and checked (`check_one` calls that found a fn).
    pub checked: u32,
    /// Rows made (each fn asked about: a checked one or a callee's head).
    pub rows: u32,
    /// Bodies live now.
    pub live: u32,
}

/// The program filled one body at a time (module docs).
pub struct LazyProgram<'n, S: NamesSource<'n> = &'n Names<'n>> {
    front: Front2<'n, S>,
    store: Store<'n>,
    buckets: Option<Buckets>,
    consts: Consts<'n>,
    prepared: &'n Prepares<'n>,
    lowerings: fill::Lowerings,
    rows: Rows<'n>,
    live: AVec<'n, Option<Live<'n>>>,
    /// Each constant a checked body read by value, by key, sorted: the seam's `const_record`.
    const_values: AVec<'n, (ItemKey, DataRecord<Global>)>,
    /// Each static a checked body named (and each static their records point at), evaluated
    /// once, sorted by key: the seam's `static_item` and `static_records` (fill.rs
    /// `body_statics`).
    statics: crate::front2::statics::Statics,
    /// The object table of each `dyn` principal a checked body's types name, and of every
    /// supertrait under it, made once per trait: the seam's `object_trait` (fill.rs
    /// `body_objects`).
    objects: HashMap<ItemKey, ObjectTable, Global>,
    /// Each async block's state recipe, made with its owner body's check (front2
    /// coroutine_recipes.rs `recipes_of`, as `Front2Program` makes them), sorted by (owner,
    /// ordinal): the seam's `coroutine_recipe`. Kept past `done` (a later instance's state of the
    /// same block is laid from it); a few words a block.
    recipes: Vec<crate::front2::coroutine_recipes::RecipeRow, Global>,
    /// Where the process starts (front2 start.rs `Entry2`), decided by [`LazyProgram::decide_start`]
    /// (program.rs `start_of`, the one start rule): the seam's `rust_start`. `None` until then.
    start: Option<crate::front2::start::Entry2>,
    /// The seam's own lang answers ([`SEAM_LANGS`]), looked up when the program is made.
    langs: [(LangItem, Option<ItemKey>); SEAM_LANGS.len()],
    /// The program's entry row: set by the first `check_one` of the key given as the entry.
    entry_key: Option<ItemKey>,
    projection_why: RefCell<Option<Box<str>>>,
    /// What the instance questions' worlds asked and found unmade (the seam's `make_asked`).
    asked: RefCell<Vec<crate::front2::world_items::Fill, Global>>,
    /// What the instance questions' worlds could not answer and no fill makes (a refused
    /// candidate impl, by name): `make_asked`'s refusal when no fill is left (rule 19).
    asked_gaps: RefCell<Vec<crate::front2::world_items::Gap, Global>>,
    /// Each crate's `#[define_opaque]` definers, read once per crate (tait.rs `Definers`).
    definers: crate::front2::tait::Definers<'static>,
    /// Each internal-symbol declaration bound to its definition once (front_end.rs
    /// `InternalBindings`; the rule is seam_rules.rs's, shared with the whole program).
    internal_bindings: RefCell<front_end::InternalBindings<'n>>,
    /// Each crate's members keyed as their containers' heads key them, made the first time a
    /// key of the crate is asked that the key map does not know (lazy/locate.rs).
    member_rows: HashMap<usize, locate::MemberIndex<'n>, Global>,
    /// Monomorphized specializations of generic `inline for` functions (`FrontEnd::specialized_row`,
    /// `FrontEnd::specialize_inline_for`), created on demand per `(orig, concrete_args)`.
    inline_for_specs: AVec<'n, crate::front2::inline_for::SpecRow>,
    counts: LazyCounts,
    alloc: Alloc<'n>,
}

impl<'n> LazyProgram<'n> {
    /// Over the names this command solved: `entry` is the key of the fn the process starts at
    /// (`main`, or the exported entry), which nothing here lowers until `check_one` asks it.
    /// `alloc` is `'static` because the head store's tables are (`Store::new`).
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], entry: Option<ItemKey>, alloc: Alloc<'static>) -> Result<LazyProgram<'n>, Front2Refusal> {
        let front = Front2::new(names, frame, user, alloc).map_err(Front2Refusal::Front)?;
        Self::over(front, entry, alloc)
    }
}

impl<'n> LazyProgram<'n, &'n crate::front2::source::Records<'n>> {
    /// Over each crate's records: `Front2::from_records` reads only the lang rows of
    /// `lang_crates`; every other crate's names record is opened on the first question about
    /// it. What a record could not answer is in `records.errors()`: the CALLER reads it after
    /// each `check_one` (the seam has no source), and a body checked over a log that is not
    /// empty is void.
    pub fn from_records(
        records: &'n crate::front2::source::Records<'n>,
        frame: &'n blaze_mem::Frame,
        lang_crates: &[usize],
        entry: Option<ItemKey>,
        alloc: Alloc<'static>,
    ) -> Result<LazyProgram<'n, &'n crate::front2::source::Records<'n>>, Front2Refusal> {
        let front = Front2::from_records(records, frame, lang_crates).map_err(Front2Refusal::Front)?;
        Self::over(front, entry, alloc)
    }

    /// The run's sources a body's macro calls expand with (the crate's environment, texts and
    /// proc-macro runner: [`crate::front2::body_host::BodySources`]), set once by the maker
    /// before any body, as `ProjectWorld::set_body_sources`; until then every body `env!`,
    /// include and proc macro is refused by name.
    pub fn set_body_sources(&mut self, sources: &'n dyn crate::front2::body_host::BodySources) {
        self.store.sources = Some(sources);
    }
}

impl<'n, S: PositionSource<'n>> LazyProgram<'n, S> {
    fn over(front: Front2<'n, S>, entry: Option<ItemKey>, alloc: Alloc<'static>) -> Result<LazyProgram<'n, S>, Front2Refusal> {
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        store.pointers = super::super::receiver::Pointers::find(&front.index, front.resolver);
        let mut langs = [(LangItem::Drop, None); SEAM_LANGS.len()];
        for (slot, item) in langs.iter_mut().zip(SEAM_LANGS) {
            // As `lang_of` in program.rs, over any source.
            let found = super::super::langs::find(&front.index, front.resolver, item).map_err(|why| Front2Refusal::Lang { item, why })?;
            *slot = (item, found.map(checker_of));
        }
        let bump = front.frame.alloc(blaze_own::Erased::new(front.frame)).bump();
        Ok(LazyProgram {
            front,
            store,
            buckets: None,
            consts: Consts::default(),
            prepared: Prepares::new_in(bump),
            lowerings: fill::Lowerings::default(),
            rows: Rows::new_in(alloc),
            live: AVec::new_in(alloc),
            const_values: AVec::new_in(alloc),
            statics: crate::front2::statics::Statics { values: Vec::new_in(Global), kinds: Vec::new_in(Global), foreign: Vec::new_in(Global) },
            objects: HashMap::new_in(Global),
            recipes: Vec::new_in(Global),
            start: None,
            langs,
            entry_key: entry,
            projection_why: RefCell::new(None),
            asked: RefCell::new(Vec::new_in(Global)),
            asked_gaps: RefCell::new(Vec::new_in(Global)),
            definers: crate::front2::tait::Definers::new_in(alloc),
            internal_bindings: RefCell::new(front_end::InternalBindings::new_in(alloc)),
            member_rows: HashMap::new_in(Global),
            inline_for_specs: AVec::new_in(alloc),
            counts: LazyCounts::default(),
            alloc,
        })
    }

    /// How many statics and object tables (supertraits included) the bodies checked so far
    /// made: for the tests (front2_lazy_data.rs).
    pub fn data_counts(&self) -> (usize, usize) {
        (self.statics.kinds.len(), self.objects.len())
    }

    /// What the program did so far.
    pub fn counts(&self) -> LazyCounts {
        self.counts
    }

    /// The front end, for a test that reads the names source (`Records::opened`).
    pub fn front(&self) -> &Front2<'n, S> {
        &self.front
    }

    /// The checker's records over this program's parts: one borrow, as `const_at` makes them.
    fn records(&mut self, consts: bool) -> Records<'_, 'n, S> {
        Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: if consts { Some((&mut self.consts, self.prepared)) } else { None },
        }
    }

    /// Row `i`'s live body, `None` when none is made or it was done.
    fn live_of(&self, i: usize) -> Option<&Live<'n>> {
        let slot = (*self.rows.live.get(i)?)?;
        self.live.get(slot as usize)?.as_ref()
    }

    /// The live body of the fn `owner`, by key.
    fn live_by_key(&self, owner: ItemKey) -> Option<&Live<'n>> {
        self.live_of(*self.rows.by_key.get(&owner)? as usize)
    }

    /// A lang item of [`SEAM_LANGS`], as looked up when the program was made.
    fn made_lang(&self, item: LangItem) -> Option<ItemKey> {
        self.langs.iter().find(|(l, _)| *l == item).and_then(|(_, k)| *k)
    }

    /// If `key` is a synthetic generic `inline for` specialization key, returns the original
    /// generic function's key.
    fn spec_orig(&self, key: ItemKey) -> Option<ItemKey> {
        for s in self.inline_for_specs.iter() {
            if self.rows.key.get(s.row).copied() == Some(key) {
                return Some(s.orig);
            }
        }
        None
    }
}
