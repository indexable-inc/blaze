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
/// Command-scoped shared immutable/sharded front-end cache across parallel workers (Phase 3).
pub mod shared;
/// The read side of a body's typed record and its test double (D163 part 2).
pub mod typed;
/// The per-fn seam rules both programs answer by (rule 30), `Front2Program`'s calls included.
pub(in crate::front2::program) mod seam_rules;
pub use seam_rules::{PositionSource, body_place};
pub use shared::{SharedLazyFront, SharedLazyFrontStats};

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

/// Shared handle to a command-scoped [`shared::SharedLazyFront`], cloned once per worker epoch
/// and dropped deterministically when [`LazyProgram`] drops.
pub type SharedLazyFrontHandle = std::sync::Arc<shared::SharedLazyFront, blaze_own::SharedAlloc<'static>>;

type SharedFrontSlot = RefCell<Option<SharedLazyFrontHandle>>;

/// The program filled one body at a time (module docs).
struct LazyConsts<'n, S> {
    names: S,
    /// Lang-crate scope copied from `front.index.scope()` so `index` is built lazily on first
    /// uncached constant evaluation instead of duplicating `Index::from_source` on every
    /// `LazyProgram::from_records` construction.
    scope: AVec<'static, usize>,
    index: RefCell<Option<crate::front2::index::Index<'n>>>,
    prepared: &'n Prepares<'n>,
    /// Persistent impl-filing index reused across constant evaluations of frozen heads so
    /// every crate's unfiled impl rows are filed at most once per lazy program instead of
    /// once per constant evaluation.
    bucket_index: RefCell<Option<crate::front2::buckets::BucketIndex>>,
    /// Evaluated constant bit patterns reused across frozen heads in this program.
    cached_bits: RefCell<blaze_hash::HashMap<blaze_id::ItemKey, u128, Alloc<'static>>>,
    /// Optional shared front-end cache attached to the owning [`LazyProgram`].
    shared: &'n SharedFrontSlot,
}

struct SharedLazyConsts<'n, S>(&'n RefCell<Option<LazyConsts<'n, S>>>);

impl<'n, S: NamesSource<'n>> crate::front2::tables::head::ConstValues<'n> for SharedLazyConsts<'n, S> {
    fn eval(
        &mut self,
        store: &mut Store<'n>,
        keys: &mut crate::front2::keys::Keys,
        c: blaze_id::ItemKey,
    ) -> Result<(), crate::front2::consts::ConstFail> {
        let guard = self.0.borrow();
        let Some(consts) = guard.as_ref() else {
            let item = checker_of(c);
            return Err(crate::front2::consts::ConstFail::Body { item, error: BodyError::NoBody(item) });
        };
        let cacheable = store.maps.const_proofs.is_none()
            && !store.maps.qpaths.borrow().is_leaf(c)
            && !store.maps.inherents.borrow().has(c);
        if cacheable
            && let Some(&bits) = consts.cached_bits.borrow().get(&c)
        {
            store.maps.file_bits(c, bits);
            return Ok(());
        }
        let shared = consts.shared.borrow().clone();
        if cacheable
            && let Some(ref shared) = shared
            && let Some(bits) = shared.get_const_bit(c)
        {
            consts.cached_bits.borrow_mut().insert(c, bits);
            store.maps.file_bits(c, bits);
            return Ok(());
        }
        if consts.index.borrow().is_none() {
            let built = crate::front2::index::Index::from_source(consts.names, consts.scope.as_slice()).map_err(|_| {
                let item = checker_of(c);
                crate::front2::consts::ConstFail::Body { item, error: BodyError::NoBody(item) }
            })?;
            *consts.index.borrow_mut() = Some(built);
        }
        let index_guard = consts.index.borrow();
        let Some(consts_index) = index_guard.as_ref() else {
            let item = checker_of(c);
            return Err(crate::front2::consts::ConstFail::Body { item, error: BodyError::NoBody(item) });
        };
        let reused = consts.bucket_index.borrow_mut().take();
        let mut buckets = Some(match reused {
            Some(ix) => Buckets::from_index(ix),
            None => Buckets::for_source(consts.names, consts_index),
        });
        let mut evaluator = Consts::default();
        let mut r = crate::front2::check::Records {
            names: consts.names,
            keys,
            index: consts_index,
            store,
            buckets: &mut buckets,
            order: crate::front2::check::BuildOrder::AsAsked,
            consts: Some((&mut evaluator, consts.prepared)),
        };
        let result = crate::front2::check::eval_len(&mut r, c);
        if let Some(b) = buckets {
            let mut ix = b.into_index();
            ix.clear_for_store();
            let mut slot = consts.bucket_index.borrow_mut();
            if slot.as_ref().is_none_or(|old| ix.unfiled_crates_len() >= old.unfiled_crates_len()) {
                *slot = Some(ix);
            }
        }
        if result.is_ok()
            && cacheable
            && let Some(&bits) = store.maps.const_bits.get(&c)
        {
            consts.cached_bits.borrow_mut().insert(c, bits);
            if let Some(ref shared) = shared {
                shared.put_const_bit(c, bits);
            }
        }
        result
    }
}

/// Cap on scratch transfers before [`LazySeed`] drops its secondary [`DirectSeedState`] when a
/// [`SharedLazyFront`] is attached, keeping per-worker scratch memory strictly bounded (Rule 18).
const DIRECT_SEED_RESET_TRANSFERS: u32 = 256;
const DIRECT_SEED_RESET_TYS: usize = 32_768;

/// Persistent in-process scratch store for [`LazySeed`], initialized lazily on the first `&Store`
/// head miss from `BodyWorld::store_want` / `BodyWorld::impl_question`. Keeps lowered transitive
/// dependencies warm across queries and transfers only the requested cell into `&Store` via
/// [`crate::front2::tables::head::transfer_cell_in_process`] (zero `BZFZ` framing, zero
/// `make_record_lives`, zero BLAKE3 digests).
struct DirectSeedState<'n> {
    store: blaze_own::Box<Store<'n>, Alloc<'static>>,
    keys: crate::front2::keys::Keys,
    enc_scratch: crate::front2::head_record::EncodeScratch<'static>,
    buf: blaze_surface::RecordBytes<'static>,
    import_cache: crate::front2::tables::head::FrozenImportCache<'n>,
    failed: blaze_hash::HashSet<[u8; crate::front2::tables::head::WANT_BYTES], Alloc<'static>>,
    transfers: u32,
}

/// Imports a single non-body-local `cell` from `shared.heads` into `store` if present, reusing
/// `import_cache` for member listings and fn-text verification proofs. Returns `true` if `cell`
/// is now made in `store`.
pub(super) fn try_import_cell_from_shared<'n, S: NamesSource<'n>>(
    names: S,
    shared: &shared::SharedLazyFront,
    store: &Store<'n>,
    keys: &crate::front2::keys::Keys,
    need: crate::front2::heads::Want,
    import_cache: &crate::front2::tables::head::FrozenImportCache<'n>,
    scratch: Alloc<'static>,
) -> bool {
    use crate::front2::tables::head::{FrozenHead, cell_of, file_frozen_cached, want_bytes};
    if store.made(need) {
        return true;
    }
    if crate::front2::tables::seeds::of_the_body(store, need) {
        return false;
    }
    let Ok(cell) = cell_of(names, need) else {
        return false;
    };
    if store.made(cell) {
        return store.made(need);
    }
    let key = want_bytes(cell);
    let Some(bytes) = shared.get_head(&key) else {
        return false;
    };
    let Ok(frozen) = FrozenHead::read(bytes.as_slice()) else {
        return false;
    };
    if file_frozen_cached(names, cell, frozen, store, keys, import_cache, scratch).is_err() {
        return false;
    }
    store.made(need)
}

/// Imports `need` (along with any parent container/trait heads required for `MemberFn` or
/// `Members`) from `shared.heads` into `store`. Returns `true` iff `need` is now made in `store`.
pub(super) fn try_import_need_from_shared<'n, S: NamesSource<'n>>(
    names: S,
    shared: &shared::SharedLazyFront,
    store: &Store<'n>,
    keys: &crate::front2::keys::Keys,
    need: crate::front2::heads::Want,
    import_cache: &crate::front2::tables::head::FrozenImportCache<'n>,
    scratch: Alloc<'static>,
) -> bool {
    use crate::front2::heads::Want;
    if crate::front2::tables::seeds::of_the_body(store, need) {
        return false;
    }
    match need {
        Want::MemberFn(container, _) => {
            let parent = Want::Head(container);
            if !store.made(parent) && !try_import_cell_from_shared(names, shared, store, keys, parent, import_cache, scratch) {
                return false;
            }
            if let Some(imp) = store.impls.get(&checker_of(container))
                && let Some(trait_) = imp.trait_
                && let Some(trait_lib) = keys.library_in(names, trait_)
            {
                let members = Want::Members(trait_lib);
                if !store.made(members) && !try_import_cell_from_shared(names, shared, store, keys, members, import_cache, scratch) {
                    return false;
                }
            }
        }
        Want::Members(container) => {
            let parent = Want::Head(container);
            if !store.made(parent) && !try_import_cell_from_shared(names, shared, store, keys, parent, import_cache, scratch) {
                return false;
            }
        }
        _ => {}
    }
    try_import_cell_from_shared(names, shared, store, keys, need, import_cache, scratch)
}

/// Freezes `cell` (already made in `store`) into canonical `BZFZ` bytes and publishes it into
/// `shared.heads` if absent.
pub(super) fn publish_cell_to_shared<'n, S: NamesSource<'n>>(
    names: S,
    shared: &shared::SharedLazyFront,
    store: &mut Store<'n>,
    keys: &crate::front2::keys::Keys,
    need: crate::front2::heads::Want,
    scratch: Alloc<'static>,
) {
    use crate::front2::tables::head::{Plant, cell_of, export, want_bytes};
    if crate::front2::tables::seeds::of_the_body(store, need) {
        return;
    }
    let Ok(cell) = cell_of(names, need) else {
        return;
    };
    if !store.made(cell) {
        return;
    }
    let key = want_bytes(cell);
    if shared.contains_head(&key) {
        return;
    }
    if let Ok(bytes) = export(names, keys, store, cell, Plant::None, scratch) {
        shared.put_head(key, bytes.as_slice());
    }
}

/// On-the-spot head seeder for [`LazyProgram`]: lowers missing heads requested through `&Store`
/// (`BodyWorld::store_want` and `BodyWorld::impl_question`) in a single persistent scratch store
/// and transfers each cell directly into `&Store` in place. When a [`SharedLazyFront`] is
/// attached, checks its sharded `BZFZ` cache first (avoiding even allocating `DirectSeedState` on
/// hits) and publishes any newly lowered cell so other workers and future epochs reuse it.
struct LazySeed<'n, S> {
    names: S,
    sized: Option<LibKey>,
    pointers: crate::front2::receiver::Pointers,
    frame: &'n blaze_mem::Frame,
    alloc: Alloc<'static>,
    state: RefCell<Option<DirectSeedState<'n>>>,
    import_cache: RefCell<Option<crate::front2::tables::head::FrozenImportCache<'n>>>,
    consts: &'n RefCell<Option<LazyConsts<'n, S>>>,
    shared: &'n SharedFrontSlot,
}

impl<'n, S> LazySeed<'n, S> {
    fn clear(&self) {
        if let Ok(mut guard) = self.state.try_borrow_mut() {
            let _ = guard.take();
        }
        if let Ok(mut guard) = self.import_cache.try_borrow_mut() {
            let _ = guard.take();
        }
        if let Ok(mut guard) = self.consts.try_borrow_mut() {
            let _ = guard.take();
        }
        if let Ok(mut guard) = self.shared.try_borrow_mut() {
            let _ = guard.take();
        }
    }
}

impl<'n, S: NamesSource<'n>> LazySeed<'n, S> {
    fn shared_handle(&self) -> Option<SharedLazyFrontHandle> {
        self.shared.borrow().clone()
    }

    fn ensure_in_scratch(
        &self,
        state: &mut DirectSeedState<'n>,
        cell: crate::front2::heads::Want,
    ) -> Result<(), ()> {
        use crate::front2::heads::{HeadError, Want};
        use crate::front2::tables::head::{ConstValues as _, MOST_FREEZE_CONSTS};
        use crate::front2::types::Why;

        if state.store.made(cell) {
            return Ok(());
        }
        if let Some(shared) = self.shared_handle()
            && try_import_cell_from_shared(self.names, &shared, &state.store, &state.keys, cell, &state.import_cache, self.alloc)
        {
            return Ok(());
        }
        let mut consts_eval = SharedLazyConsts(self.consts);
        let mut asked: AVec<'_, LibKey> = AVec::new_in(self.alloc);
        for _ in 0..=MOST_FREEZE_CONSTS {
            let c = match state.store.ensure(self.names, &mut state.keys, cell) {
                Ok(()) => return Ok(()),
                Err(HeadError::Item(_, Why::NeedConst(c, _))) => c,
                Err(_) => return Err(()),
            };
            if asked.contains(&c) || asked.len() == MOST_FREEZE_CONSTS {
                return Err(());
            }
            asked.push(c);
            if self.names.seeded_text_of(c).is_some() && !state.store.made(Want::Fn(c)) {
                if state.keys.item(c).is_err() {
                    return Err(());
                }
                if state.store.ensure(self.names, &mut state.keys, Want::Fn(c)).is_err() {
                    return Err(());
                }
            }
            if consts_eval.eval(&mut state.store, &mut state.keys, c).is_err() {
                return Err(());
            }
        }
        Err(())
    }

    fn transfer_one(
        &self,
        state: &mut DirectSeedState<'n>,
        store: &Store<'n>,
        keys: &crate::front2::keys::Keys,
        need: crate::front2::heads::Want,
        scratch: Alloc<'static>,
    ) -> Result<(), ()> {
        use crate::front2::tables::head::{cell_of, transfer_cell_in_process, want_bytes};
        let cell = cell_of(self.names, need).map_err(|_| ())?;
        if store.made(cell) {
            return Ok(());
        }
        let shared = self.shared_handle();
        if let Some(ref shared) = shared
            && try_import_cell_from_shared(self.names, shared, store, keys, cell, &state.import_cache, scratch)
        {
            return Ok(());
        }
        let key = want_bytes(cell);
        if state.failed.contains(&key) {
            return Err(());
        }
        if self.ensure_in_scratch(state, cell).is_err() {
            state.failed.insert(key);
            return Err(());
        }
        if let Some(ref shared) = shared {
            publish_cell_to_shared(self.names, shared, &mut state.store, &state.keys, cell, scratch);
        }
        if transfer_cell_in_process(
            self.names,
            cell,
            &state.store,
            &state.keys,
            store,
            keys,
            &mut state.enc_scratch,
            &mut state.buf,
            &state.import_cache,
            scratch,
        )
        .is_err()
        {
            state.failed.insert(key);
            return Err(());
        }
        state.transfers = state.transfers.saturating_add(1);
        Ok(())
    }
}

impl<'n, S: NamesSource<'n>> crate::front2::tables::seeds::HeadSeed<'n> for LazySeed<'n, S> {
    fn seed(
        &self,
        store: &Store<'n>,
        keys: &crate::front2::keys::Keys,
        need: crate::front2::heads::Want,
        scratch: Alloc<'static>,
    ) -> Result<crate::front2::heads::Seeded, crate::front2::tables::seeds::SeedError> {
        use crate::front2::heads::{Seeded, Want};
        if crate::front2::tables::seeds::of_the_body(store, need) {
            return Ok(Seeded::Lower);
        }
        if store.made(need) {
            return Ok(Seeded::Filed);
        }
        let shared = self.shared_handle();
        // Fast path: hit the command-wide sharded frozen-head cache without allocating a
        // secondary per-worker `DirectSeedState` store.
        if let Some(ref shared) = shared
            && let Ok(mut cache_guard) = self.import_cache.try_borrow_mut()
        {
            let import_cache = cache_guard.get_or_insert_with(|| crate::front2::tables::head::FrozenImportCache::new(self.alloc));
            if try_import_need_from_shared(self.names, shared, store, keys, need, import_cache, scratch) {
                return Ok(Seeded::Filed);
            }
        }
        let Ok(mut guard) = self.state.try_borrow_mut() else {
            return Ok(Seeded::Lower);
        };
        let state = guard.get_or_insert_with(|| {
            let mut s = blaze_own::Box::new_in(Store::new(self.sized, self.frame, self.alloc), self.alloc);
            s.pointers = self.pointers;
            s.seed_on_ensure = false;
            DirectSeedState {
                store: s,
                keys: crate::front2::keys::Keys::new_in(self.alloc),
                enc_scratch: crate::front2::head_record::EncodeScratch::new_in(self.alloc),
                buf: blaze_surface::RecordBytes::new_in(self.alloc),
                import_cache: crate::front2::tables::head::FrozenImportCache::new(self.alloc),
                failed: blaze_hash::HashSet::new_in(self.alloc),
                transfers: 0,
            }
        });
        state.store.prepared = store.prepared;
        state.store.sources = store.sources;
        // A member fn's body lowering (`lower_member` -> `own_trait::of_member`) reads its
        // container's head (`store.impls` / `store.trait_defs`), and `track_caller` for a trait
        // impl method reads the trait's declared member (`keys.filed_member`) and trait head
        // (`store.trait_defs`), so seed parent heads alongside member cells.
        let mut ok = true;
        match need {
            Want::MemberFn(container, _) => {
                let parent = Want::Head(container);
                if !store.made(parent)
                    && !crate::front2::tables::seeds::of_the_body(store, parent)
                    && self.transfer_one(state, store, keys, parent, scratch).is_err()
                {
                    ok = false;
                }
                if ok
                    && let Some(imp) = store.impls.get(&checker_of(container))
                    && let Some(trait_) = imp.trait_
                    && let Some(trait_lib) = keys.library_in(self.names, trait_)
                {
                    let trait_head = Want::Head(trait_lib);
                    if !store.made(trait_head)
                        && !crate::front2::tables::seeds::of_the_body(store, trait_head)
                        && self.transfer_one(state, store, keys, trait_head, scratch).is_err()
                    {
                        ok = false;
                    }
                    let members = Want::Members(trait_lib);
                    if ok
                        && !store.made(members)
                        && !crate::front2::tables::seeds::of_the_body(store, members)
                        && self.transfer_one(state, store, keys, members, scratch).is_err()
                    {
                        ok = false;
                    }
                }
            }
            Want::Members(container) => {
                let parent = Want::Head(container);
                if !store.made(parent)
                    && !crate::front2::tables::seeds::of_the_body(store, parent)
                    && self.transfer_one(state, store, keys, parent, scratch).is_err()
                {
                    ok = false;
                }
            }
            _ => {}
        }
        let res = if ok && self.transfer_one(state, store, keys, need, scratch).is_ok() {
            Seeded::Filed
        } else {
            Seeded::Lower
        };
        // Bound secondary scratch store memory when backed by `SharedLazyFront`: freshly lowered
        // cells are already published into `shared.heads`, so dropping `DirectSeedState` frees its
        // heap tables cleanly.
        if shared.is_some()
            && (state.transfers >= DIRECT_SEED_RESET_TRANSFERS || state.store.tys.view().len() >= DIRECT_SEED_RESET_TYS)
        {
            let _ = guard.take();
        }
        Ok(res)
    }
}

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
    /// Per-program memoization cache for `caller_site`, `def_site`, and macro expansion
    /// placement (seam_rules.rs `SiteCache`).
    site_cache: RefCell<seam_rules::SiteCache<'n>>,
    /// Each crate's members keyed as their containers' heads key them, made the first time a
    /// key of the crate is asked that the key map and targeted container probes do not know
    /// (lazy/locate.rs).
    member_rows: Map<usize, locate::MemberIndex<'n>, Alloc<'n>>,
    /// Per-crate targeted member index over `#[lang]` containers alone, built lazily before
    /// falling back to a full crate-wide `member_rows` scan so `core`/`std` lang trait methods
    /// (e.g. `PartialEq::eq` in `assert_eq!`) resolve in O(lang items) with zero `each_item` scan.
    lang_member_rows: Map<usize, locate::MemberIndex<'n>, Alloc<'n>>,
    /// Per-crate cache of body-local item derivation chains built once on the first unhinted
    /// `locate_local` miss in a crate (lazy/locate.rs), turning repeated O(crate_fns) lex-scans
    /// into O(1) lookups.
    local_rows: Map<usize, locate::LocalIndex<'n>, Alloc<'n>>,
    /// Monomorphized specializations of generic `inline for` functions (`FrontEnd::specialized_row`,
    /// `FrontEnd::specialize_inline_for`), created on demand per `(orig, concrete_args)`.
    inline_for_specs: AVec<'n, crate::front2::inline_for::SpecRow>,
    /// Prebuilt fixed spelling table reused by every `in_world` query (ground-type trait
    /// selection/normalization never resolves written body paths, avoiding O(names) HashMap
    /// rebuilds on every query).
    fixed_spell: Spelling<'static>,
    /// Append-only scan cursors into `self.store.tys.nodes()` for `adt_rows` and `body_objects`,
    /// preventing O(N^2) prefix rescans across checked bodies.
    adt_scanned_tys: usize,
    object_scanned_tys: usize,
    /// Frame-allocated [`LazySeed`] wired into `self.store.seeds`; cleared on drop so all
    /// heap-backed [`SeededHeads`] buffers are released with the program.
    seed: Option<&'n LazySeed<'n, S>>,
    /// Frame-allocated cell holding an optional command-wide [`SharedLazyFrontHandle`], shared
    /// with [`LazySeed`] and [`LazyConsts`] so [`Self::attach_shared`] wires all three at once
    /// and [`Drop`] deterministically releases the `Arc` refcount.
    shared: &'n SharedFrontSlot,
    /// Reusable import cache for [`shared::SharedLazyFront`] frozen-head imports in `make_want`.
    shared_import_cache: RefCell<Option<crate::front2::tables::head::FrozenImportCache<'n>>>,
    counts: LazyCounts,
    alloc: Alloc<'n>,
}

impl<'n, S: NamesSource<'n>> Drop for LazyProgram<'n, S> {
    fn drop(&mut self) {
        if let Some(seed) = self.seed.take() {
            seed.clear();
        }
        if let Ok(mut guard) = self.shared.try_borrow_mut() {
            let _ = guard.take();
        }
        let _ = self.shared_import_cache.get_mut().take();
    }
}

impl<'n> LazyProgram<'n> {
    /// Over the names this command solved: `entry` is the key of the fn the process starts at
    /// (`main`, or the exported entry), which nothing here lowers until `check_one` asks it.
    /// `alloc` is `'static` because the head store's tables are (`Store::new`).
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], entry: Option<ItemKey>, alloc: Alloc<'static>) -> Result<LazyProgram<'n>, Front2Refusal> {
        let front = Front2::new(names, frame, user, alloc).map_err(Front2Refusal::Front)?;
        Self::over(front, entry, alloc, false)
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
        Self::over(front, entry, alloc, true)
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
    fn over(front: Front2<'n, S>, entry: Option<ItemKey>, alloc: Alloc<'static>, seeded_heads: bool) -> Result<LazyProgram<'n, S>, Front2Refusal> {
        let sized = front.index.lang("sized");
        let pointers = super::super::receiver::Pointers::find(&front.index, front.resolver);
        let mut store = Store::new(sized, front.frame, alloc);
        store.pointers = pointers;
        let bump = front.frame.alloc(blaze_own::Erased::new(front.frame)).bump();
        let prepared = Prepares::new_in(bump);
        store.prepared = Some(prepared);
        let shared_slot: &'n SharedFrontSlot = front.frame.alloc(RefCell::new(None));
        let (seed, buckets) = if seeded_heads {
            let mut scope = AVec::with_capacity_in(front.index.scope().len(), alloc);
            scope.extend_from_slice(front.index.scope());
            let consts_cell: &'n RefCell<Option<LazyConsts<'n, S>>> = front.frame.alloc(RefCell::new(Some(LazyConsts {
                names: front.names,
                scope,
                index: RefCell::new(None),
                prepared,
                bucket_index: RefCell::new(None),
                cached_bits: RefCell::new(blaze_hash::HashMap::new_in(alloc)),
                shared: shared_slot,
            })));
            let seed: &'n LazySeed<'n, S> = front.frame.alloc(LazySeed {
                names: front.names,
                sized,
                pointers,
                frame: front.frame,
                alloc,
                state: RefCell::new(None),
                import_cache: RefCell::new(None),
                consts: consts_cell,
                shared: shared_slot,
            });
            store.seeds = Some(seed);
            store.seed_on_ensure = false;
            (Some(seed), Some(Buckets::for_source(front.names, &front.index)))
        } else {
            (None, None)
        };
        let mut langs = [(LangItem::Drop, None); SEAM_LANGS.len()];
        for (slot, item) in langs.iter_mut().zip(SEAM_LANGS) {
            // As `lang_of` in program.rs, over any source.
            let found = super::super::langs::find(&front.index, front.resolver, item).map_err(|why| Front2Refusal::Lang { item, why })?;
            *slot = (item, found.map(checker_of));
        }
        Ok(LazyProgram {
            front,
            store,
            buckets,
            consts: Consts::default(),
            prepared,
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
            site_cache: RefCell::new(seam_rules::SiteCache::new_in(alloc)),
            member_rows: Map::new_in(alloc),
            lang_member_rows: Map::new_in(alloc),
            local_rows: Map::new_in(alloc),
            inline_for_specs: AVec::new_in(alloc),
            fixed_spell: Spelling::fixed(),
            adt_scanned_tys: 0,
            object_scanned_tys: 0,
            seed,
            shared: shared_slot,
            shared_import_cache: RefCell::new(None),
            counts: LazyCounts::default(),
            alloc,
        })
    }

    /// Attaches a command-scoped [`SharedLazyFrontHandle`] cache shared across parallel export
    /// workers and worker epochs. Idempotent: subsequent calls after the first are no-ops.
    pub fn attach_shared(&self, shared: &SharedLazyFrontHandle) {
        let mut slot = self.shared.borrow_mut();
        if slot.is_none() {
            *slot = Some(std::sync::Arc::clone(shared));
        }
    }

    /// Returns a cloned handle to the attached command-scoped [`shared::SharedLazyFront`], if any.
    #[inline]
    pub(super) fn shared(&self) -> Option<SharedLazyFrontHandle> {
        self.shared.borrow().clone()
    }

    /// Returns `(live_bodies, store_ty_nodes)` currently retained in this [`LazyProgram`], used by
    /// parallel export workers to bound per-worker heap RSS via clean epoch resets while
    /// [`shared::SharedLazyFront`] retains shared heads, constants, member indexes, and typed data.
    pub fn worker_epoch_load(&self) -> (usize, usize) {
        (self.counts.live as usize, self.store.tys.view().len())
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
