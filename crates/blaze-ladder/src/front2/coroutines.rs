//! Coroutines, the front end's answers to the code builder (docs/v2/plan/design/coroutines.md,
//! "Builder writer 5's notes": the seams the front end implements; "Front-end writer 6's
//! notes": this file). The Reference, "Async blocks": "An async block is a variant of a block
//! expression which evaluates to a future." "Async functions": `async fn f(x: A) -> T { body }`
//! "is roughly equivalent to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`.
//!
//! A coroutine's IDENTITY is a closure's (one concept, one home: blaze-build coroutine.rs):
//! the owner fn whose body holds the `async` block, the block's ordinal among that body's
//! async blocks, the owner's ground arguments. Everything here is keyed by it and is generic
//! over the seam ([`FrontEnd`]): the run path's driver (run.rs) holds one [`Coroutines`] per
//! build and answers the builder from it.
//!
//! - [`CoroutineTable`]: `LayoutFacts::coroutine`, each block's upvars and output, made once
//!   from its owner's body and check record by `blaze_build::coroutine_facts` (the upvars are
//!   the rows of `Checked::captures` whose first column is the block: blaze-typeck
//!   rules/async_fn.rs writes them).
//! - [`States`]: `LayoutFacts::coroutine_state`. A state is laid out from TYPES before any
//!   build (run/lay_states.rs, ASYNC.md P8) and recorded here; a state asked that was not laid
//!   is `Missing` naming the owner, which the run refuses by name (never a second build).
//! - [`opaque_hidden`]: `LayoutFacts::opaque_hidden`. An opaque type's hidden type is its
//!   defining body's (`Checked::hidden`); an async block's future is given in its identity
//!   form, at the block's own ordinal.
//! - [`async_items`], [`future_poll`]: `Adts::async_items`, `Adts::future_poll`, from the
//!   library's `#[lang]` marks (the pinned core: `"Poll"` task/poll.rs, `"pin"` pin.rs,
//!   `"poll"` future/future.rs, `"coroutine_resume"` ops/coroutine.rs) and `IntoFuture` by
//!   its path.
//! - [`part_of`], [`owner_of`], [`key_args`]: the three functions of one coroutine instance
//!   (`Future::poll`, the resume fn, `drop_in_place`, each at `[coroutine]`) are tabled as
//!   their OWNER's, as a closure's `Fn*` method is, and told apart by the item in the key.
//!
//! Refused by name, never guessed: an opaque type whose defining fn is not in the program or
//! gave no hidden type; a coroutine whose state holds itself by value.

use core::cell::RefCell;

use blaze_build::layout::{LayoutFacts, Missing};
use blaze_build::{Adts, AsyncItems, BuildError, CoroutineFacts, CoroutineOf, Instance, StateShape};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{Checked, ItemKey, LangItem, List, Name, PNode, PTy, PortableTys};

use crate::front::FnLang;
use crate::seam::FrontEnd;

/// Every async block's facts ([`LayoutFacts::coroutine`]), sorted by (owner, ordinal). A block
/// whose facts were refused (a capture's type that does not ground) has its row with the
/// refusal: the builder then refuses the block by name.
pub(crate) struct CoroutineTable {
    tys: PortableTys<'static>,
    rows: Vec<(ItemKey, u32, Result<(PTy, PTy), BuildError>), Alloc<'static>>,
}

impl CoroutineTable {
    /// The table of every checked body of `program` (`checked[i]` is fn `i`'s record).
    pub(crate) fn of<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>], alloc: Alloc<'static>) -> CoroutineTable {
        let mut table = CoroutineTable::new_in(alloc);
        for (i, done) in checked.iter().enumerate() {
            let Some(done) = done else { continue };
            table.add(program, i, done, alloc);
        }
        table
    }

    /// An empty table, its columns in `alloc` (rule 75).
    pub(crate) fn new_in(alloc: Alloc<'static>) -> CoroutineTable {
        CoroutineTable { tys: PortableTys::new_in(alloc), rows: Vec::new_in(alloc) }
    }

    /// Adds the async blocks of fn `i`'s checked body `done`: the ONE home of a row's making
    /// ([`Self::of`] is one `add` per body; the run path adds each body as it checks, never
    /// rebuilding the table). Each row is placed by (owner, ordinal) with `partition_point` and
    /// an insert, so the table stays sorted for `get`. Returns the rows added.
    pub(crate) fn add<P: FrontEnd>(&mut self, program: &P, i: usize, done: &Checked<'static>, alloc: Alloc<'static>) -> usize {
        let function = program.function(i);
        let Some((body, _)) = function.body else { return 0 };
        let view = body.view(program.body_types(i));
        let sig = program.sig(i);
        let generics = sig.parent_count + sig.own_count;
        let mut added = 0;
        for (ordinal, _) in blaze_build::coroutines_in(&view) {
            let facts = blaze_build::coroutine_facts(&view, done, ordinal, generics, &mut self.tys, alloc);
            let key = (function.key, ordinal);
            let at = self.rows.partition_point(|r| (r.0, r.1) < key);
            self.rows.insert(at, (function.key, ordinal, facts));
            added += 1;
        }
        added
    }

    /// The facts of block `ordinal` of `owner`; `None` when the owner has no such block or
    /// its facts were refused.
    pub(crate) fn get(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
        let at = self.rows.binary_search_by_key(&(owner, ordinal), |r| (r.0, r.1)).ok()?;
        let (env, output) = *self.rows[at].2.as_ref().ok()?;
        Some(CoroutineFacts { types: self.tys.view(), env, output })
    }

    /// Whether `owner` has a block `ordinal` at all (its facts made or refused).
    pub(crate) fn has(&self, owner: ItemKey, ordinal: u32) -> bool {
        self.rows.binary_search_by_key(&(owner, ordinal), |r| (r.0, r.1)).is_ok()
    }
}

struct StatesInner {
    /// Each state made: its instance (its arguments are `args[start..start + len]`) and shape.
    made: Vec<(ItemKey, u32, u32, u32, StateShape), Alloc<'static>>,
    args: Vec<PTy, Alloc<'static>>,
}

impl StatesInner {
    fn find(&self, owner: ItemKey, ordinal: u32, args: &[PTy]) -> Option<StateShape> {
        self.made
            .iter()
            .find(|m| m.0 == owner && m.1 == ordinal && self.args.get(m.2 as usize..(m.2 + m.3) as usize) == Some(args))
            .map(|m| m.4)
    }
}

/// The states of the coroutine instances built so far ([`LayoutFacts::coroutine_state`]).
/// Behind a `RefCell`: the builder asks through `&self` while the driver, between builds,
/// records. One thread, and no borrow is held across a call out of this file.
pub(crate) struct States {
    inner: RefCell<StatesInner>,
}

impl States {
    pub(crate) fn new(alloc: Alloc<'static>) -> States {
        let inner = StatesInner { made: Vec::new_in(alloc), args: Vec::new_in(alloc) };
        States { inner: RefCell::new(inner) }
    }

    /// The state of `owner`'s block `ordinal` at `args`. Every state a build reads was laid
    /// before it (run/lay_states.rs): one not laid is `Missing` naming the owner, which the run
    /// refuses by name (never a second build).
    pub(crate) fn get(&self, owner: ItemKey, ordinal: u32, args: &[PTy]) -> Result<StateShape, Missing> {
        self.inner.borrow().find(owner, ordinal, args).ok_or(Missing { item: owner })
    }

    /// Whether the state of that instance is made.
    pub(crate) fn made(&self, owner: ItemKey, ordinal: u32, args: &[PTy]) -> bool {
        self.inner.borrow().find(owner, ordinal, args).is_some()
    }

    /// Records the state `shape` laid out for that instance (run/lay_states.rs).
    pub(crate) fn record(&self, owner: ItemKey, ordinal: u32, args: &[PTy], shape: StateShape) {
        let mut s = self.inner.borrow_mut();
        if s.find(owner, ordinal, args).is_some() {
            return;
        }
        // Both counts fit: a ground table holds at most `u32::MAX` types.
        let (start, len) = (s.args.len() as u32, args.len() as u32);
        s.args.extend_from_slice(args);
        s.made.push((owner, ordinal, start, len, shape));
    }

    /// Forgets the state recorded for that instance: the lay that recorded it stopped before
    /// building it into its state (run/lay_states.rs), so the next lay lays and builds it again
    /// instead of taking it as made with no functions built.
    pub(crate) fn forget(&self, owner: ItemKey, ordinal: u32, args: &[PTy]) {
        let mut s = self.inner.borrow_mut();
        let StatesInner { made, args: all } = &mut *s;
        made.retain(|m| !(m.0 == owner && m.1 == ordinal && all.get(m.2 as usize..(m.2 + m.3) as usize) == Some(args)));
    }
}

/// What one build holds of the program's coroutines: the facts, the states made so far, and
/// the library's items, each looked up once.
pub(crate) struct Coroutines {
    pub(crate) table: CoroutineTable,
    pub(crate) states: States,
    pub(crate) items: Option<AsyncItems>,
    pub(crate) poll: Option<ItemKey>,
}

impl Coroutines {
    pub(crate) fn of<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>], alloc: Alloc<'static>) -> Coroutines {
        Coroutines { table: CoroutineTable::of(program, checked, alloc), states: States::new(alloc), items: async_items(program), poll: future_poll(program) }
    }

    /// `LayoutFacts::coroutine_state`: `Ok(None)` for a block the table does not hold (its
    /// owner did not check: refused by name by the builder), `Missing` for one not built yet.
    pub(crate) fn state(&self, owner: ItemKey, ordinal: u32, args: &[PTy]) -> Result<Option<StateShape>, Missing> {
        if !self.table.has(owner, ordinal) {
            return Ok(None);
        }
        self.states.get(owner, ordinal, args).map(Some)
    }
}

/// `Future::poll`, the trait's own fn (`#[lang = "poll"]`, core's future/future.rs: `fn
/// poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;`). `None` when the
/// library declares none, or its lookup is refused: the builder then refuses an `.await`
/// naming `poll`.
pub(crate) fn future_poll<P: FrontEnd>(program: &P) -> Option<ItemKey> {
    program.lang_item(LangItem::FuturePoll).ok().flatten()
}

/// The library items an `.await` and a coroutine's entries are built from. `None` when any is
/// absent (a program whose library has no futures), or the `Poll` enum's row is not made (no
/// reached body names `Poll`), or its variants are not the library's `Ready` and `Pending`:
/// the builder then refuses the async block or the `.await` naming `Adts::async_items`.
pub(crate) fn async_items<P: FrontEnd>(program: &P) -> Option<AsyncItems> {
    let into_future = program.lang_item(LangItem::IntoFuture).ok().flatten()?;
    let poll = program.lang_named("Poll")?;
    let pin = program.lang_named("pin")?;
    let resume = program.lang_named("coroutine_resume")?;
    let adt = program.adt(poll)?;
    // Core's task/poll.rs: `pub enum Poll<T> { Ready(T), Pending }`: by the variants' names.
    let at = |name: &str| adt.variants.iter().position(|v| v.name == Name::of(name)).and_then(|i| u32::try_from(i).ok());
    Some(AsyncItems { into_future, poll, ready: at("Ready")?, pending: at("Pending")?, pin, resume })
}

/// `LayoutFacts::opaque_hidden`: the hidden type of `opaque` at `args` (the opaque's
/// arguments: its fn's generic parameters, parents first, as front2's heads make them), in
/// `ground`. Read from the defining fn's check record (`Checked::hidden`). A hidden type that
/// is an async block's future (an `async fn`'s root block; a fn returning `async { .. }`) is
/// written by the checker by identity, `PNode::Coroutine { owner, ordinal, Param(0..) }`
/// (async P3), which is the builder's ground form too: `ground` maps its arguments.
pub(crate) fn opaque_hidden<P: FrontEnd>(
    program: &P,
    checked: &[Option<Checked<'static>>],
    facts: &dyn LayoutFacts,
    opaque: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let refused = |what| BuildError::UnsupportedSig { what };
    let Some((owner, index)) = program.opaque_of(opaque) else {
        return Err(refused("an `impl Trait` type no fn head of the program made (the seam's `opaque_of`)"));
    };
    // A type alias's `impl Trait` (its owner row is the alias: tait.rs `of_alias`) has its
    // hidden type in the check record of the fn that defines it, `#[define_opaque(Alias)]`
    // (tait.rs `defining_fn`), never the alias's own (an alias has no body). Before (tempo
    // t1935d, 10 lines, eyre's `default_with` over std's `LazyResolve`): the alias was taken for
    // the fn and its check refused "NotAHead(.., \"a type alias\")". No trait selects an alias,
    // so no `resolve`. A generic alias's opaque is refused by name: its arguments are the
    // alias's parameters, and the defining fn's hidden type is written over the fn's own,
    // which are not mapped to them here.
    if super::tait::of_alias(index) {
        if !args.is_empty() {
            return Err(refused("a generic type alias's `impl Trait` laid out (its defining fn's parameters are not mapped to the alias's yet)"));
        }
        let definer = program.alias_definer(owner).map_err(|miss| refused(miss.form()))?;
        return defined_hidden(program, checked, facts, opaque, definer, args, ground, scratch);
    }
    // A trait fn's `impl Trait` (its `async fn`'s future) is an associated type of the trait
    // (RFC 3425; front2 rpitit.rs): at a ground `Self` its value is the SELECTED impl's, never
    // the trait fn's default body's. The impl is selected as the call is (the seam's
    // `resolve`, the one selection: the layout and the call cannot disagree); a fn of no trait,
    // and a provided fn the impl does not write, resolve to the owner itself: its own record.
    // The question is asked in the world of a fn WITH a body: a required trait fn's row (a
    // declared head, program.rs `declared`) has none, and its `in_world` is the ctor world's
    // refusal, `Missing(FnSig(owner))`, which no fill answers (the REQUIRED run test's "the
    // world has no answer to FnSig(..)"). Every argument is ground, so the selection is the
    // same in any body's world (each sees every opaque: `Reveal::All`); the first such row is
    // the deterministic one.
    let bodied = |n: &usize| program.function(*n).body.is_some();
    let Some(world) = program.fn_index(owner).filter(bodied).or_else(|| (0..program.fn_count()).find(bodied)) else {
        return Err(refused("an `impl Trait` type whose defining fn is not in the program, in a program with no body to select its impl in"));
    };
    let list = ground.intern_list(args).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    let (callee, callee_args) = program.resolve(world, owner, list, ground).map_err(BuildError::Select)?;
    if callee != owner {
        return opaque_at_impl(program, checked, facts, opaque, owner, callee, callee_args, ground, scratch);
    }
    defined_hidden(program, checked, facts, opaque, owner, args, ground, scratch)
}

/// The hidden type of `opaque` at `args` from the check record of `owner`, the fn that defines
/// it (its own return opaque, or an alias's: [`opaque_hidden`]), grounded.
#[expect(clippy::too_many_arguments, reason = "the reveal's question, its defining fn and the grounding's tables")]
fn defined_hidden<P: FrontEnd>(
    program: &P,
    checked: &[Option<Checked<'static>>],
    facts: &dyn LayoutFacts,
    opaque: ItemKey,
    owner: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let refused = |what| BuildError::UnsupportedSig { what };
    // An opaque may be met by the layout of a type that holds it, never by a call of its
    // defining fn (std's `Backtrace` holds `LazyLock<Capture, LazyResolve>`, whose
    // `#[define_opaque(LazyResolve)] fn lazy_resolve` eyre's `DefaultHandler::default_with` never
    // calls; t1800): a program that checks on demand has no row for it yet. Named, so that
    // driver checks it and builds again (run/make_one.rs).
    let Some(i) = program.fn_index(owner) else {
        return Err(BuildError::OpaqueOwnerUnchecked { opaque, owner });
    };
    let function = program.function(i);
    let (Some(_), Some(done)) = (function.body, checked.get(i).and_then(Option::as_ref)) else {
        return Err(refused("an `impl Trait` type whose defining fn has no checked body"));
    };
    let Some(&(_, hidden)) = done.hidden.iter().find(|h| h.0 == opaque) else {
        return Err(refused("an `impl Trait` type its defining body gave no hidden type (a body that never returns)"));
    };
    let from = done.types.view();
    match from.node(hidden) {
        Ok(PNode::Future(..)) => Err(BuildError::Malformed {
            what: "a hidden type naming an async block by its expression (the checker writes it by identity, `PNode::Coroutine`, since async P3)",
        }),
        Ok(_) => blaze_build::ground(from, hidden, args, ground, scratch, facts),
        Err(_) => Err(BuildError::Malformed { what: "a hidden type past its record's table" }),
    }
}

/// [`opaque_hidden`] of trait fn `owner`'s opaque `opaque` where the selection at the ground
/// arguments chose impl fn `callee` (at `callee_args`, in `ground`: the impl's arguments, then
/// the fn's own): the type at the opaque's place of the impl fn's return type (front2 rpitit.rs
/// `place_in`, the walk the impl's associated-type row is read by), grounded there. An impl fn
/// that writes `impl Trait` or `async fn` gives its own opaque, revealed from its own body's
/// record by the same grounding; one that writes a type gives it.
#[expect(clippy::too_many_arguments, reason = "the reveal's question, the selected impl fn and the grounding's tables")]
fn opaque_at_impl<P: FrontEnd>(
    program: &P,
    checked: &[Option<Checked<'static>>],
    facts: &dyn LayoutFacts,
    opaque: ItemKey,
    owner: ItemKey,
    callee: ItemKey,
    callee_args: List,
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let refused = |what| BuildError::UnsupportedSig { what };
    let (Some(t), Some(j)) = (program.fn_index(owner), program.fn_index(callee)) else {
        return Err(refused("a trait fn's `impl Trait` type whose trait fn or selected impl fn is not in the program"));
    };
    // The impl fn's own opaque is revealed from its record: one not checked yet is asked by
    // name, to be checked and the build asked again (never read as the trait's default).
    if program.function(j).body.is_some() && checked.get(j).and_then(Option::as_ref).is_none() {
        return Err(BuildError::MissingRecord { item: callee });
    }
    let (ts, is) = (program.sig(t), program.sig(j));
    let value = super::rpitit::place_in(ts.types, ts.output, is.types, is.output, opaque, scratch).map_err(refused)?;
    let mut at: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    at.extend_from_slice(ground.view().list(callee_args).map_err(|_| BuildError::Malformed { what: "a selected impl fn's arguments past the ground table" })?);
    blaze_build::ground(is.types, value, &at, ground, scratch, facts)
}

/// Which of a coroutine instance's three functions an instance is (blaze-build coroutine.rs,
/// "THE EMIT POINTS").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    /// `Future::poll` at `[coroutine]`: the entry.
    Poll,
    /// The resume fn at `[coroutine]`: the split body.
    Resume,
    /// `drop_in_place` at `[coroutine]`: the split drop.
    Drop,
}

impl Part {
    /// Its place among a built instance's three functions: poll, resume, drop.
    pub(crate) const fn index(self) -> usize {
        match self {
            Part::Poll => 0,
            Part::Resume => 1,
            Part::Drop => 2,
        }
    }
}

/// `instance` as one of a coroutine's three functions: its one argument a coroutine and its
/// item `Adts::future_poll`, `AsyncItems::resume` or `drop_in_place`. `None` for any other.
pub(crate) fn part_of(adts: &dyn Adts, instance: Instance, ground: &PortableTys<'_>) -> Option<(CoroutineOf, Part)> {
    let view = ground.view();
    let [first] = view.list(instance.args).ok()? else { return None };
    let of = blaze_build::coroutine_of(view, *first)?;
    let part = if adts.future_poll() == Some(instance.item) {
        Part::Poll
    } else if adts.async_items().map(|a| a.resume) == Some(instance.item) {
        Part::Resume
    } else if adts.lang_fn(FnLang::DropInPlace.lang()) == Some(instance.item) {
        Part::Drop
    } else {
        return None;
    };
    Some((of, part))
}

/// `called` as a coroutine's function: its owner's index, which tables it (the driver builds
/// it from the owner's body); `None` for any other call, and for a coroutine whose owner is
/// not in the program (the call is then refused as any callee not reached).
pub(crate) fn owner_of<P: FrontEnd>(program: &P, adts: &dyn Adts, called: Instance, ground: &PortableTys<'_>) -> Option<usize> {
    let (of, _) = part_of(adts, called, ground)?;
    program.fn_index(of.owner)
}

/// Whether `instance`'s first argument is a coroutine: with an item that is not its tabled
/// fn's own, it is one of the coroutine's three functions ([`key_args`] keys it).
pub(crate) fn self_is_coroutine(instance: Instance, ground: &PortableTys<'_>) -> bool {
    let view = ground.view();
    view.list(instance.args).ok().and_then(|l| l.first().copied()).and_then(|t| blaze_build::coroutine_of(view, t)).is_some()
}

/// The list one of a coroutine's three functions is keyed by: its owner's arguments first (so
/// the owner's projections normalize at them), then the item as a `FnDef` (which of the three
/// it is), then the instance's own argument (the coroutine, which holds the ordinal). Never
/// equal to the owner's own list (it is longer) nor to a closure method's (whose own
/// arguments are a closure and a tuple).
pub(crate) fn key_args(instance: Instance, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<List, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a coroutine instance's arguments past the ground table" };
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let mut own: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    own.extend_from_slice(ground.view().list(instance.args).map_err(malformed)?);
    let of = own
        .first()
        .and_then(|&t| blaze_build::coroutine_of(ground.view(), t))
        .ok_or(BuildError::Malformed { what: "a coroutine function's instance whose argument is not a coroutine" })?;
    let mut all: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    all.extend_from_slice(ground.view().list(of.owner_args).map_err(malformed)?);
    all.push(ground.intern(PNode::FnDef(instance.item, List::EMPTY)).map_err(full)?);
    all.extend_from_slice(&own);
    ground.intern_list(&all).map_err(full)
}
