//! D106 piece X: ONE missing instance made, for the record walk ([`crate::body_ids::Walk`]).
//!
//! The walk finds every instance it can by a key made from text before any check; the maker
//! here is called only for an instance whose record is not found. It checks the bodies that one
//! instance needs (through the seam's [`FrontEnd::check_one`], never `check_all`), pulls the
//! constants it reads at its ground arguments, builds it (a plain body, a closure's `Fn*`
//! method, a fn shim, one of a coroutine's three functions, or `drop_in_place`'s glue), names
//! every reference by symbol, and appends its entry: the code, and the instances it declares
//! (code_records.rs `make_entry`).
//!
//! It is a child of `run` so it calls run.rs's own per-item functions and types
//! ([`FrontAdts`], [`Bodies`], [`ClosureTable`], [`BackLangs`], `callee_refusal`, `closure_owner`)
//! without copying them and without widening their visibility.
//!
//! WHAT `build_from` DOES FOR THE WHOLE PROGRAM, AND ITS ONE-INSTANCE FORM HERE:
//! - `check_all` (every body): [`MakerState::ensure`], one `check_one` per fn key, asked once a
//!   run: the instance's tabled fn, and each owner its ground ARGUMENTS hold (`held_owners`: a
//!   closure's or coroutine's owner, whose facts the layouts read, and an opaque type's
//!   defining fn), and each async block's owner a coroutine demand names. A fn ITEM type in the
//!   arguments is not checked: this instance reads none of its body. Its row is made from its
//!   head ([`FrontEnd::head_row`]): the build reads its ABI and signature.
//! - `eval_consts` (every `const` and `static` of the program): not run. The record walk takes a
//!   front end that evaluates each constant at its arguments (`evaluates_at_args`): the per-item
//!   pull `const_at::needs_of` + `const_at::pull` for THIS instance. The old front is refused by
//!   name ([`RunError::OneBody`]).
//! - `static_image` (every static's bytes): not run. A record names a static, thread-local or
//!   vtable by symbol; each one's data is its own record (D106 item 6, bzcode03's kinds 1-3).
//! - `ClosureTable::of` (every checked body): each newly checked body adds its own closures'
//!   rows (`ClosureTable::add`, [`MakerState::ensure`]); `CoroutineTable::of` likewise: each
//!   newly checked body with an async block adds its own blocks' rows (`CoroutineTable::add`).
//! - coroutine states: run/lay_states.rs `lay_and_build`, the one path (build_from's too): laid
//!   out from types before the instance is built, each coroutine then built into its state; an
//!   owner fn the lay needs that is not checked yet is checked ([`Step::Check`]) and the
//!   instance is made again (types only were read).
//! - the reference loop and the vtable loop inside build_from's step (slots, keys): [`name_refs`]
//!   and [`vtable_rows`], by instance NAME, no slot, no instance key (the walk tells rows apart by
//!   symbol).
//!
//! THE ONE GROUND TABLE lives across instances in [`MakerState`]: coroutine states, pulled
//! constants and the coroutine stash are keyed by interned types of it, as in build_from.
//!
//! Allocators: `Alloc<'static>` handed in by the caller (the tables this calls into take
//! `Alloc<'static>`: `CoroutineTable::of`, lay_states.rs). `Global` is named only where run.rs's
//! existing types fix it (`Bodies::const_at`, `BuiltParts::args`, blaze-build's `Built<Global>`).

use super::*;

use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_own::AVec;

use crate::body_ids::{BodyIds, Counts, EntryForm, Fixture, Grant, Make, Walk, WalkError};
use crate::code_records::{Builder, Callee, CodeRecordError, CodeStore, Declared, InstanceNames, VtableDatum, make_entry_with_vtables};
use crate::front2::const_at::{ConstTable, Needs};
use crate::front2::coroutines::Coroutines;
use crate::front2::program::lazy::LazyRefusal;
use super::lay_states::{Lay, lay_and_build, roots_of_instance, unchecked_owner};
use crate::symbols::{decode_instance, encode_instance, fn_symbol_of_name, vtable_symbol};

/// How many bodies were checked for a walk's makes: the claim "a run that finds every record
/// checks no body" is read here. A plain counter, owned by the maker and read by the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CheckCount {
    /// `FrontEnd::check_one` calls that handed a body over.
    pub check_one: usize,
    /// Bodies the program held CHECKED when it was constructed (fns with a body, at
    /// construction): today's `Front2Program::new` checks its whole reach there; a lazy program
    /// holds none.
    pub by_construction: usize,
    /// Programs constructed: 0 in a run that found every record.
    pub constructed: usize,
    /// Checked bodies walked to table their async blocks ([`MakerState::ensure`]): each body
    /// with a closure or an async block is walked once, when its check hands it over
    /// (`CoroutineTable::add`), so this is at most `check_one` (the d106_records test asserts it).
    pub coroutine_walks: usize,
}

/// Work at the lazy program's body boundary for this run. An initial lowering counts
/// when it produces a body. Declarations and requests for other instances of that body
/// add none. The other two counters count calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoweringCounts {
    pub bodies_lowered: u64,
    pub lower_again: u64,
    pub forks: u64,
}

impl core::fmt::Display for LoweringCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bodies lowered {}, lower_again calls {}, forks {}", self.bodies_lowered, self.lower_again, self.forks)
    }
}

impl CheckCount {
    /// Every check the makes cost.
    pub fn all(&self) -> usize {
        self.check_one + self.by_construction
    }
}

/// THE TYPING SEAM (D163 part 2, rule 78): where the make takes a body's checked columns from.
/// [`SecondCheck`] checks the body here (today's production: the check `blaze check` made is
/// done a second time); [`FromRecord`] reads what the check kept and never checks (D159 "one
/// body record"; a gap is a typed refusal naming the body, never a second check). Both lower
/// the body here and table it the same way (lazy fill.rs `check_one_key_by`), so the oracle
/// ([`Twin`]) holds them to byte-equal entries.
///
/// THE FLIP: production's typing is the one line in blaze-cli run_parent.rs `maker` that hands
/// `SecondCheck` to `Maker::of_roots` (and the `Front::Maker` type naming it), which becomes
/// `FromRecord(<the check's typed records>)` once the check keeps them.
pub trait Typing<P: FrontEnd> {
    /// Fn `key`'s row and checked body, as the seam's [`FrontEnd::check_one`] answers.
    fn check_one(&mut self, program: &mut P, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError>;
}

/// The body checked in the make, a second time (the seam's own `check_one`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SecondCheck;

impl<P: FrontEnd> Typing<P> for SecondCheck {
    fn check_one(&mut self, program: &mut P, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        program.check_one(key)
    }
}

/// The body's checked columns read from the check's record `R` (lazy typed.rs), never checked
/// in the make. Only the lazy program answers it: the one program the record walk makes.
pub struct FromRecord<R>(pub R);

impl<'n, S: crate::front2::program::lazy::PositionSource<'n>, R: crate::front2::program::lazy::typed::TypedRecords> Typing<crate::front2::program::lazy::LazyProgram<'n, S>> for FromRecord<R> {
    fn check_one(&mut self, program: &mut crate::front2::program::lazy::LazyProgram<'n, S>, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        program.check_one_typed(key, &mut self.0)
    }
}

/// The run's tables that [`Bodies`] owns, kept between instances.
struct Tables {
    closures: ClosureTable,
    coroutines: Coroutines,
    const_at: ConstTable<Global>,
}

/// What lives across instances besides the checked bodies.
struct Work {
    alloc: Alloc<'static>,
    isa: Isa,
    overflow: OverflowChecks,
    /// What `ub_checks()` is: the records' key's debug assertions (`Maker::new`).
    ub: blaze_build::UbChecks,
    /// The program's panic strategy: the records' key's (`Maker::new`).
    panic: blaze_build::Panic,
    langs: BackLangs,
    ground: PortableTys<'static>,
    layouts: Layouts<Alloc<'static>>,
    /// Each coroutine instance built, its three functions waiting for their instances.
    stash: Vec<BuiltParts, Global>,
    needs: Needs<Global>,
    /// Whether a made instance touched a thread-local: the program then needs the getter's
    /// and ender's records (code_records.rs `RecordRun::getter`/`ender`).
    thread_locals: bool,
    /// The program's compiler_builtins soft-float routines as instances, found once a run
    /// (front2/builtins_shim.rs): every instance's emitter gets their places.
    routines: crate::front2::builtins_shim::Bound,
}

/// One maker's state over program `P` (made with the program, on the first miss).
pub struct MakerState {
    /// Each fn key asked of `check_one`, sorted: the fn's index, `None` when it had no body.
    asked: AVec<'static, (ItemKey, Option<usize>)>,
    /// Fn `i`'s checked body, `None` until asked (or with none): what [`Bodies`] reads.
    checked: AVec<'static, Option<Checked<'static>>>,
    tables: Option<Tables>,
    work: Work,
}

/// The step's outcome inside one borrow of [`Bodies`].
enum Step {
    Entry,
    /// The lay of a coroutine state met owner fn `.0` not checked yet (lay_states.rs
    /// `Lay::Unchecked`): check it, then make the instance again.
    Check(ItemKey),
    /// Fns the build's references resolved to that no row tables yet ([`Named1::Untabled`]):
    /// check each (which tables its row), then make the instance again; each with the refusal
    /// given when its check tables none.
    Table(Vec<(ItemKey, RunError), Global>),
}

fn tables_gone(key: ItemKey) -> RunError {
    RunError::OneBody { key, why: "the maker's tables were left taken by an earlier failed step (make_one.rs `MakerState::tables`)" }
}

impl MakerState {
    /// Build a static datum or TLS template using the same ground names as code entries.
    pub fn static_entry<P: FrontEnd>(
        &mut self,
        program: &P,
        value: &crate::seam::SeamData,
        kind: StaticItem,
        template: bool,
    ) -> Result<AVec<'static, u8>, crate::code_records::statics::StaticRecordError> {
        use crate::code_records::statics::{make_static_entry_with_targets, make_template_entry_with_targets};

        let alloc = self.work.alloc;
        let ground = &mut self.work.ground;
        let fn_target = |key| {
            let Some(instance) = program.const_fn_instance(key, ground, alloc) else {
                return Ok(None);
            };
            let view = ground.view();
            let tabled = static_fn_tabled(program, instance, view);
            let mut name = blaze_own::Vec::new_in(alloc);
            encode_instance(instance.item, instance.args, view, alloc, &mut name)?;
            Ok(Some((tabled, name)))
        };
        if template {
            make_template_entry_with_targets(value, kind, fn_target, alloc)
        } else {
            make_static_entry_with_targets(value, kind, fn_target, alloc)
        }
    }

    /// The state for `program`, before its first instance. Refuses a front end that evaluates
    /// constants for the whole program only (`evaluates_at_args` false), naming `first`.
    pub fn new<P: FrontEnd>(program: &P, first: ItemKey, isa: Isa, overflow: OverflowChecks, ub: blaze_build::UbChecks, panic: blaze_build::Panic, alloc: Alloc<'static>) -> Result<MakerState, RunError> {
        if !program.evaluates_at_args() {
            return Err(RunError::OneBody {
                key: first,
                why: "this front end evaluates its constants for the whole program (run.rs `eval_consts`); the record walk takes one that evaluates each at its arguments (front2)",
            });
        }
        let langs = BackLangs::of(program)?;
        let empty: &[Option<Checked<'static>>] = &[];
        let tables = Tables {
            closures: ClosureTable::of(program, empty, alloc),
            coroutines: Coroutines::of(program, empty, alloc),
            const_at: ConstTable::new_in(Global),
        };
        let ground = PortableTys::new_in(alloc);
        let routines = crate::front2::builtins_shim::Bound::new(program.soft_float_routines(alloc), &ground, alloc).map_err(|_| RunError::OneBody {
            key: first,
            why: "a compiler_builtins soft-float routine's instance name did not encode (front2/builtins_shim.rs `Bound::new`)",
        })?;
        let work = Work {
            alloc,
            isa,
            overflow,
            ub,
            panic,
            langs,
            ground,
            layouts: Layouts::new_in(alloc),
            stash: Vec::new_in(Global),
            needs: Needs::new_in(Global),
            thread_locals: false,
            routines,
        };
        Ok(MakerState { asked: blaze_own::Vec::new_in(alloc), checked: blaze_own::Vec::new_in(alloc), tables: Some(tables), work })
    }

    /// Fn `key` checked, once a run: its index, `None` when the front end has no body for it.
    /// A newly checked body with a closure or an async block re-makes the closure and coroutine
    /// tables over the bodies checked so far. Every OTHER fn whose closure a checked body's
    /// types hold is checked too, before this returns (its closure's facts are in its body):
    /// by a work list, each owner once.
    fn ensure<P: FrontEnd, T: Typing<P>>(&mut self, program: &mut P, typing: &mut T, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        /// The most closure owners one ensure follows (rule 18).
        const MOST_OWNERS: usize = 4096;
        let first = self.ensure_one(program, typing, key, checks)?;
        let mut todo: AVec<'_, ItemKey> = blaze_own::Vec::new_in(self.work.alloc);
        self.owners_named(&*program, key, &mut todo);
        let mut steps = 0;
        while let Some(owner) = todo.pop() {
            if self.asked.binary_search_by_key(&owner, |r| r.0).is_ok() {
                continue;
            }
            steps += 1;
            if steps > MOST_OWNERS {
                return Err(RunError::OneBody { key, why: "more closure owners named than the most one ensure follows" });
            }
            self.ensure_one(program, typing, owner, checks)?;
            self.owners_named(&*program, owner, &mut todo);
        }
        Ok(first)
    }

    /// Each fn other than `key` whose check `key`'s checked body's types read, pushed on `todo`
    /// (in node order: deterministic for one body): a closure's owner (its facts are in its
    /// body), and an `impl Trait` type's defining fn (the seam's `opaque_of`: its hidden type is
    /// in that fn's check, front2/coroutines.rs `opaque_hidden`), an `async fn`'s future among
    /// them.
    fn owners_named<P: FrontEnd>(&self, program: &P, key: ItemKey, todo: &mut AVec<'_, ItemKey>) {
        let Some(Some(i)) = self.asked.binary_search_by_key(&key, |r| r.0).ok().map(|n| self.asked[n].1) else { return };
        let Some(Some(done)) = self.checked.get(i) else { return };
        let view = done.types.view();
        for n in view.nodes() {
            let owner = match *n {
                blaze_typeck::PNode::Closure { owner, .. } => owner,
                blaze_typeck::PNode::Opaque(opaque, _) => match program.opaque_of(opaque) {
                    Some((owner, _)) => owner,
                    None => continue,
                },
                _ => continue,
            };
            if owner != key {
                todo.push(owner);
            }
        }
    }

    /// [`MakerState::ensure`] of one key, owners not followed.
    fn ensure_one<P: FrontEnd, T: Typing<P>>(&mut self, program: &mut P, typing: &mut T, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        let slot = match self.asked.binary_search_by_key(&key, |r| r.0) {
            Ok(n) => return Ok(self.asked[n].1),
            Err(n) => n,
        };
        // A ctor has no body to check: its row is made, and its instance built as a fn
        // ([`build_one`]'s ctor arm).
        // The trait's `Clone::clone` has no body either: the one instance tabled at it is a
        // builtin clone ([`build_one`]'s arm; clone_shim.rs `language_copy_clone`).
        if program.ctor_of(key).is_some() || program.lang_trait_fn(LangItem::Clone, "clone") == Some(key) {
            let at = program.head_row(key)?;
            self.asked.insert_shift(slot, (key, at));
            return Ok(at);
        }
        // The body's checked columns by the typing seam: checked again, or read (`Typing`).
        let got = typing.check_one(program, key)?;
        let at = match got {
            None => None,
            Some((i, done)) => {
                checks.check_one += 1;
                while self.checked.len() < program.fn_count().max(i + 1) {
                    self.checked.push(None);
                }
                let mut refresh = done.closures.len() != 0;
                if !refresh && let Some((body, _)) = program.function(i).body {
                    let view = body.view(program.body_types(i));
                    refresh = blaze_build::coroutines_in(&view).into_iter().next().is_some();
                }
                self.checked[i] = Some(done);
                if refresh {
                    let tables = self.tables.as_mut().ok_or_else(|| tables_gone(key))?;
                    // This body's closures, one row each ([`ClosureTable::add`]): the table is
                    // never re-made over the bodies checked before.
                    if let (Some((body, _)), Some(done)) = (program.function(i).body, self.checked[i].as_ref()) {
                        let sig = program.sig(i);
                        tables.closures.add(&body.view(program.body_types(i)), done, sig.parent_count + sig.own_count, self.work.alloc);
                    }
                    // This body's async blocks, one row each (front2/coroutines.rs
                    // `CoroutineTable::add`): the table is never re-made over the bodies checked
                    // before, so each checked body is walked once (counted).
                    if let Some(done) = self.checked[i].as_ref() {
                        checks.coroutine_walks += 1;
                        tables.coroutines.table.add(&*program, i, done, self.work.alloc);
                    }
                    // The library's async items, read again with the rows made so far: before the
                    // first check a lazy front end has no `Poll` row yet, so `MakerState::new`'s
                    // reading is `None` (front2/coroutines.rs `async_items`).
                    tables.coroutines.items = crate::front2::coroutines::async_items(&*program);
                    tables.coroutines.poll = crate::front2::coroutines::future_poll(&*program);
                }
                Some(i)
            }
        };
        self.asked.insert_shift(slot, (key, at));
        Ok(at)
    }
}

/// Reification records a selected fn item. Closure coercion records `call_once` at the
/// closure type, whose body belongs to the item that contains the closure expression.
pub(super) fn static_fn_tabled<P: FrontEnd>(program: &P, instance: Instance, ground: blaze_typeck::PTys<'_>) -> ItemKey {
    if let Ok([first, _]) = ground.list(instance.args)
        && let Some(closure) = blaze_build::closure_of(ground, *first)
    {
        let (lang, name) = crate::front2::langs::fn_trait_method(blaze_build::FnTrait::FnOnce);
        if program.lang_trait_fn(lang, name) == Some(instance.item) {
            return closure.owner;
        }
    }
    instance.item
}

/// Makes ONE instance: `name` (symbols.rs `encode_instance` bytes), tabled as fn `tabled`;
/// appends its entry (code_records.rs `make_entry`) to `entry`. The one generic step of D106:
/// every front end behind the seam is made through it (rule 78). `typing`: where each body's
/// checked columns come from ([`Typing`]).
pub fn make_instance<P: FrontEnd, T: Typing<P>>(
    program: &mut P,
    typing: &mut T,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    name: &[u8],
    entry: &mut AVec<'_, u8>,
) -> Result<(), RunError> {
    let alloc = state.work.alloc;
    // The bodies this ONE instance reads: its fn's, and each owner its name holds.
    let Some(i) = state.ensure(program, typing, tabled, checks)? else {
        return Err(RunError::OneBody { key: tabled, why: "the record walk reached an instance of a fn the front end holds no checked body for" });
    };
    let (item, args) = decode_instance(name, &mut state.work.ground, alloc).map_err(|e| RunError::CodeRecord { function: program.function(i).named(), error: CodeRecordError::Symbol(e) })?;
    let instance = Instance { item, args };
    // Each fn whose check another fact of this instance's ARGUMENTS is in: a closure's or an
    // async block's owner (its capture facts are in its body: `LayoutFacts::closure`, which
    // lay_states.rs `roots_of_instance` reads before the build), and an `impl Trait`'s
    // defining fn (its hidden type), as [`MakerState::owners_named`] follows them for a
    // checked body's types. Read from the ground tree, not by the program's rows: a lazy front
    // end has no row for an owner until it is checked (front2/program/lazy `fn_index`), so a
    // filter by row skipped `rand::rngs::thread::rng` for `LocalKey::try_with` at its closure.
    let (owners, items) = held_owners(&*program, state.work.ground.view(), args, tabled, alloc)?;
    for &owner in owners.iter() {
        state.ensure(program, typing, owner, checks)?;
    }
    // Each fn item whose TYPE the arguments hold: its row from its head, its body not checked
    // (this instance reads none of it; its own instance is the walk's, found or made). The
    // Reference, "Function item types": "When referred to, a function item, or the
    // constructor of a tuple-like struct or enum variant, yields a zero-sized value of its
    // function item type. That type explicitly identifies the function", so calling the value
    // calls that fn with its own ABI and signature, which the build asks (`Adts::abi`) as it
    // builds the call `f(x)` at `F` = the item's type, before any reference is named. The body
    // that named the item as a value may have been found by its record and not checked in
    // this run, so no check made that head (t1530, 36 tests: "the build's `Adts` gave no ABI
    // for it" at `Result::map` with `NonZero::<u64>::new`, `Option::map` with `Into::into`,
    // fastrand's `try_with_rng(Rng::fork)`, a fn handed to `OnceLock::get_or_init`).
    for &item in items.iter() {
        program.head_row(item)?;
    }
    // The constants this instance reads at its ground arguments, pulled before the build
    // (front2/const_at.rs): a plain instance's own body only, as in build_from.
    pull_constants(program, state, i, instance)?;
    // The build, its coroutine states laid out from types first (lay_states.rs); a lay that
    // meets an owner fn not checked yet has it checked and the instance made again (no LIR
    // was built: types only were read); bounded (rule 18).
    let mut owners = 0usize;
    // An instance-time question that met an answer no check made (`Missing`): the program
    // makes what its worlds asked (`FrontEnd::make_asked`) and the instance is built again,
    // as build_from's step is; bounded (rule 18), and nothing made is the `Missing` itself.
    let mut asked = 0u32;
    loop {
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
let step = build_one(&*program, &bodies, work, i, instance, entry);
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        let step = match step {
            Err(e) if asks_again(&e) && asked < crate::front2::check::MOST_ROUNDS => {
                asked += 1;
                if program.make_asked(i)? {
                    continue;
                }
                return Err(e);
            }
            // A layout met a type alias's `impl Trait` whose defining fn is not checked yet
            // (coroutines.rs `opaque_hidden`): that fn is checked, as a lay's owner is
            // (`Step::Check`), and the instance made again; bounded by the same count, and an
            // owner asked before is the refusal itself (its check made no row).
            Err(RunError::Build { error: BuildError::OpaqueOwnerUnchecked { owner, .. }, .. })
                if owners < MOST_HELD_COROUTINES && state.asked.binary_search_by_key(&owner, |r| r.0).is_err() =>
            {
                owners += 1;
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    return Err(unchecked_owner(&*program, i, owner));
                }
                continue;
            }
            step => step?,
        };
        match step {
            Step::Entry => break,
            Step::Check(owner) => {
                owners += 1;
                if owners > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "an instance whose coroutine states needed more than 4096 owner fns checked" };
                    return Err(RunError::Build { function: program.function(i).named(), error, range: None });
                }
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    return Err(unchecked_owner(&*program, i, owner));
                }
            }
            Step::Table(keys) => {
                for (key, refusal) in keys {
                    owners += 1;
                    // Asked before: its check tabled no row, so the build met it untabled again.
                    if owners > MOST_HELD_COROUTINES || state.asked.binary_search_by_key(&key, |r| r.0).is_ok() {
                        return Err(refusal);
                    }
                    state.ensure(program, typing, key, checks)?;
                }
            }
        }
    }
    // No `done` here: the walk may reach another instance of this fn at other arguments (a
    // generic fn, `pick` at `i32` then at `u8`), whose make reads the same lowered body, and a
    // body is checked once a run (the lazy program refuses a second `check_one`). The body is
    // held for the run beside its checked result (`MakerState::checked`, held for the run too).
    Ok(())
}

/// The owners the ground arguments `args` of an instance of `tabled` hold, each once, in the
/// order first met: a closure's or a coroutine's owner, and an opaque type's defining fn; never
/// `tabled` itself (checked first); and `.1`, each fn item whose type they hold (`PNode::FnDef`,
/// a closure's parent arguments included), each once, whose row the make takes from its head.
/// Each distinct node is visited once (ground types are interned: one id per type), so the
/// walk is linear in the arguments' distinct nodes; bounded (rule 18).
fn held_owners<'a, P: FrontEnd>(program: &P, ground: blaze_typeck::PTys<'_>, args: blaze_typeck::List, tabled: ItemKey, alloc: Alloc<'a>) -> Result<(AVec<'a, ItemKey>, AVec<'a, ItemKey>), RunError> {
    /// The most distinct type nodes one instance's arguments are walked through (rule 18).
    const MOST_NODES: usize = 1 << 20;
    let malformed = |_| RunError::OneBody { key: tabled, why: "an instance's ground arguments refer past their table (make_one.rs `held_owners`)" };
    let mut owners: AVec<'a, ItemKey> = blaze_own::Vec::new_in(alloc);
    let mut items: AVec<'a, ItemKey> = blaze_own::Vec::new_in(alloc);
    let mut seen: AVec<'a, blaze_typeck::PTy> = blaze_own::Vec::new_in(alloc);
    let mut todo: AVec<'a, blaze_typeck::PTy> = blaze_own::Vec::new_in(alloc);
    todo.extend_from_slice(ground.list(args).map_err(malformed)?);
    while let Some(t) = todo.pop() {
        let Err(slot) = seen.binary_search(&t) else { continue };
        if seen.len() >= MOST_NODES {
            return Err(RunError::OneBody { key: tabled, why: "an instance whose ground arguments hold more distinct types than the most one make walks (2^20)" });
        }
        seen.insert_shift(slot, t);
        let node = ground.node(t).map_err(malformed)?;
        if let blaze_typeck::PNode::FnDef(item, _) = node
            && !items.contains(&item)
        {
            items.push(item);
        }
        let owner = match node {
            blaze_typeck::PNode::Closure { owner, .. } | blaze_typeck::PNode::Coroutine { owner, .. } => Some(owner),
            blaze_typeck::PNode::Opaque(opaque, _) => program.opaque_of(opaque).map(|(owner, _)| owner),
            _ => None,
        };
        if let Some(owner) = owner
            && owner != tabled
            && !owners.contains(&owner)
        {
            owners.push(owner);
        }
        ground.children_of(node, &mut todo).map_err(malformed)?;
    }
    Ok((owners, items))
}

/// The per-item constant pull of build_from's step (`const_at::needs_of`, `const_at::pull`).
fn pull_constants<P: FrontEnd>(program: &mut P, state: &mut MakerState, i: usize, instance: Instance) -> Result<(), RunError> {
    let MakerState { checked, tables, work, .. } = &mut *state;
    work.needs.clear();
    let function = program.function(i);
    // A ctor's fn reads no constant: its fields in, its value out. Nor a builtin clone (a copy).
    if instance.item != function.key
        || (function.kind == FnKind::Ctor && function.body.is_none())
        || (function.body.is_none() && crate::clone_shim::language_copy_clone(&*program, instance, &work.ground).is_some())
    {
        return Ok(());
    }
    let (Some((body, _)), Some(done)) = (function.body, checked.get(i).and_then(Option::as_ref)) else {
        return Err(callee_refusal(&*program, function.named(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view()));
    };
    let malformed = |_| RunError::Build { function: function.named(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    args.extend_from_slice(work.ground.view().list(instance.args).map_err(malformed)?);
    let t = tables.take().ok_or_else(|| tables_gone(function.key))?;
    let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
    let view = body.view(program.body_types(i));
    let facts = FrontAdts(&*program, i, &[], &work.langs, &bodies);
    let found = crate::front2::const_at::needs_of(&view, done, &args, &mut work.ground, &facts, work.alloc, &mut work.needs);
    *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
    found.map_err(|error| RunError::Build { function: function.named(), error, range: None })?;
    if work.needs.is_empty() {
        return Ok(());
    }
    let named = function.named();
    let table = tables.as_mut().ok_or_else(|| tables_gone(instance.item))?;
    if let Err(refusal) = crate::front2::const_at::pull(&mut *program, &work.needs, work.ground.view(), &mut table.const_at, &table.closures) {
        let item = refusal.need.item();
        let path = program.item_named(item);
        return Err(RunError::ConstAt { function: named, refusal, path });
    }
    Ok(())
}

/// build_from's step for one instance, from "A closure's `Fn*` method instance" to
/// `make_entry`, over one borrow of the bodies.
fn build_one<P: FrontEnd>(program: &P, bodies: &Bodies<'_>, work: &mut Work, i: usize, instance: Instance, entry: &mut AVec<'_, u8>) -> Result<Step, RunError> {
    let function = program.function(i);
    let name = || function.named();
    // A ctor's row has no body: its own instance is built as a fn, and a `Fn*` shim of it
    // (`.map(S)`) as any fn item's (as run.rs `build_from`).
    let ctor_row = match (function.kind, function.body) {
        (FnKind::Ctor, None) => program.ctor_of(function.key).map(|(_, variant)| variant),
        _ => None,
    };
    // The trait's bodiless `Clone::clone` at a fn item or fn pointer: the language's copy
    // (clone_shim.rs `language_copy_clone`), built with no body.
    let builtin_clone = match function.body {
        None if instance.item == function.key => crate::clone_shim::language_copy_clone(program, instance, &work.ground),
        _ => None,
    };
    let checked = match (function.body, bodies.checked.get(i).and_then(Option::as_ref)) {
        (Some((body, _)), Some(done)) => Some((body, done)),
        _ if ctor_row.is_some() || builtin_clone.is_some() => None,
        _ => return Err(callee_refusal(program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view())),
    };
    let malformed = |_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    let adts = FrontAdts(program, i, &[], &work.langs, bodies);
    let coroutine = if instance.item == function.key { None } else { crate::front2::coroutines::part_of(&adts, instance, &work.ground) };
    let shim = if instance.item == function.key || coroutine.is_some() { None } else { crate::fn_shim::fn_shim_of(&adts, instance, &work.ground) };
    let closure = if instance.item == function.key || shim.is_some() || coroutine.is_some() {
        None
    } else {
        let first = work.ground.view().list(instance.args).map_err(malformed)?.first().copied();
        let of = first.and_then(|t| blaze_build::closure_of(work.ground.view(), t));
        let which = [blaze_build::FnTrait::Fn, blaze_build::FnTrait::FnMut, blaze_build::FnTrait::FnOnce]
            .into_iter()
            .find(|&w| adts.fn_trait_method(w) == Some(instance.item));
        match (of, which) {
            (Some(of), Some(which)) => Some((of, which)),
            _ => return Err(callee_refusal(program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NotOwnInstance, work.ground.view())),
        }
    };
    let owner_args = match shim {
        Some(s) => s.item.map_or(List::EMPTY, |(_, fn_args)| fn_args),
        None => match coroutine {
            Some((of, _)) => of.owner_args,
            None => closure.map_or(instance.args, |(of, _)| of.owner_args),
        },
    };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    args.extend_from_slice(work.ground.view().list(owner_args).map_err(malformed)?);
    // One of a coroutine's three functions: its instance is built once for all three. The part
    // is READ in the stash, (stash row, part) here, and leaves it only when its entry is made
    // (the end of this fn): a make that stops first (`Step::Table`, `Step::Check`) is made again
    // and reads the same part.
    let twice = || RunError::Build { function: name(), error: BuildError::Malformed { what: "a coroutine's function its instance's build did not leave (asked for twice)" }, range: None };
    let from_coroutine: Option<(usize, usize)> = match coroutine {
        None => None,
        Some((of, part)) => {
            let find = |stash: &[BuiltParts]| stash.iter().position(|b| b.owner == of.owner && b.ordinal == of.ordinal && b.args[..] == args[..]);
            if find(&work.stash[..]).is_none() {
                // Its state laid out from types, with every state it holds, and each built into
                // its state (lay_states.rs).
                if let Lay::Unchecked(owner) = lay_and_build(program, &[], &work.langs, bodies, &mut work.ground, &mut work.layouts, &mut work.stash, i, &[of], work.alloc)? {
                    return Ok(Step::Check(owner));
                }
            }
            let Some(at) = find(&work.stash[..]) else {
                let error = BuildError::Malformed { what: "a coroutine laid out and built whose functions are not in the stash (lay_states.rs)" };
                return Err(RunError::Build { function: name(), error, range: None });
            };
            if work.stash[at].parts[part.index()].is_none() {
                return Err(twice());
            }
            Some((at, part.index()))
        }
    };
    // The states this instance's own types hold, laid out from types and each coroutine built
    // into its state BEFORE this build (lay_states.rs): the build never meets a state not laid.
    if from_coroutine.is_none() && shim.is_none() {
        let roots = roots_of_instance(program, bodies, &adts, i, &args, &mut work.ground, work.alloc)?;
        if !roots.is_empty()
            && let Lay::Unchecked(owner) = lay_and_build(program, &[], &work.langs, bodies, &mut work.ground, &mut work.layouts, &mut work.stash, i, &roots, work.alloc)?
        {
            return Ok(Step::Check(owner));
        }
    }
    let view = checked.map(|(body, done)| (body.view(program.body_types(i)), done));
    let own = if from_coroutine.is_some() {
        None
    } else {
        let mut subst = Subst { args: &args, ground: &mut work.ground, scratch: work.alloc };
        let made = if let Some(s) = shim {
            match s.item {
                Some(_) => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, Some(&program.sig(i)), &mut subst, &adts, Global),
                None => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, None, &mut subst, &adts, Global),
            }
        } else if let Some(clone) = builtin_clone {
            // `Self: Copy` holds for a fn item and a fn pointer by the language (no solver
            // asked): the clone is a copy of its bytes.
            blaze_build::build_builtin_clone(clone, true, &mut subst, &adts, Global)
        } else if let Some(variant) = ctor_row {
            // The ctor's own instance (a shim of it was taken above): its fields in, its value out.
            blaze_build::build_ctor_fn(function.key, variant, &program.sig(i), &mut subst, &adts, Global)
        } else if let Some((view, done)) = view.as_ref() {
            if let Some((of, which)) = closure {
                blaze_build::build_closure(view, done, of.ordinal, which, &program.sig(i), &mut subst, &adts, Global)
            } else if Some(function.key) == program.fn_lang(FnLang::DropInPlace) {
                blaze_build::build_drop_glue(view, done, &program.sig(i), &mut subst, &adts, Global)
            } else {
                blaze_build::build(view, done, &program.sig(i), &mut subst, &adts, Global)
            }
        } else {
            Err(BuildError::Malformed { what: "a row with no body built as other than a ctor or its shim" })
        };
        Some(made.map_err(|error| build_refusal(program, name(), checked.map(|(body, _)| body), error))?)
    };
    let built: &blaze_build::Built<Global> = match (&own, from_coroutine) {
        (Some(b), _) => b,
        (None, Some((at, part))) => work.stash[at].parts[part].as_ref().ok_or_else(twice)?,
        (None, None) => return Err(twice()),
    };
    // Every reference named; each instance it reaches declared by (tabled fn, name).
    work.thread_locals |= !built.thread_locals.is_empty();
    let mut reach = Reach::new_in(work.alloc);
    let mut funcs: AVec<'_, Callee<'_>> = blaze_own::Vec::with_capacity_in(built.refs.len(), work.alloc);
    let mut untabled: Vec<(ItemKey, RunError), Global> = Vec::new_in(Global);
    for &called in built.refs.iter() {
        match name_ref(program, &adts, &mut work.ground, work.alloc, &mut reach, i, instance.args, called)? {
            Named1::Instance(symbol) => funcs.push(Callee::Symbol(symbol)),
            Named1::C(c_name) => funcs.push(Callee::C(c_name)),
            Named1::Untabled(key, refusal) => {
                if !untabled.iter().any(|(k, _)| *k == key) {
                    untabled.push((key, refusal));
                }
            }
        }
    }
    let mut vtable_data: AVec<'_, (blaze_code::record::Symbol, AVec<'static, u8>)> = blaze_own::Vec::new_in(work.alloc);
    for &want in built.vtables.iter() {
        vtable_rows(program, &adts, &mut work.ground, &mut work.layouts, work.alloc, &mut reach, i, instance.args, want, &mut untabled, &mut vtable_data)?;
    }
    // Every untabled callee of this build at once (its calls' and its vtables' words): checked,
    // then the instance made again.
    if !untabled.is_empty() {
        return Ok(Step::Table(untabled));
    }
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    let mut vtables: AVec<'_, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(built.vtables.len(), work.alloc);
    for &table in built.vtables.iter() {
        vtables.push(vtable_symbol(table, work.ground.view(), work.alloc).map_err(sym)?);
    }
    // The soft-float routines after the instance's own references: the emitter's one table
    // (front2/builtins_shim.rs); `make_entry` declares those it says the record calls.
    let routines = work.routines.bind(&mut funcs).map_err(|error| RunError::CodeRecord { function: name(), error })?;
    let names = InstanceNames { funcs: &funcs, statics: &built.statics, vtables: &vtables, tls: &built.thread_locals };
    let mut declared: AVec<'_, Declared<'_>> = blaze_own::Vec::with_capacity_in(reach.rows.len(), work.alloc);
    for n in 0..reach.rows.len() {
        let (tabled, name_bytes) = reach.row(n);
        declared.push(Declared { tabled, name: name_bytes });
    }
    let mut carried: AVec<'_, VtableDatum<'_>> = blaze_own::Vec::with_capacity_in(vtable_data.len(), work.alloc);
    for (symbol, datum) in vtable_data.iter() {
        carried.push(VtableDatum { symbol: *symbol, datum: datum.as_slice() });
    }
    let made = make_entry_with_vtables(built.func.verified(), &names, &mut carried, routines, &mut declared, &crate::code_records::reads::ReadList::empty(), &built.traps, work.isa, work.alloc)
        .map_err(|error| RunError::CodeRecord { function: name(), error })?;
    entry.extend_from_slice(&made);
    // The coroutine's part leaves the stash with its entry made, not before.
    if let Some((at, part)) = from_coroutine {
        work.stash[at].parts[part] = None;
    }
    Ok(Step::Entry)
}

/// build_from's mapping of a build error to the run's refusal, by the body's ranges (a ctor's
/// fn has no body, so no range).
fn build_refusal<P: FrontEnd>(program: &P, function: Named, body: Option<&blaze_lower::typeck::TypeckBody<'static>>, error: BuildError) -> RunError {
    let ranges = |all: &[blaze_typeck::Range], i: u32| all.get(i as usize).copied();
    let range = match error {
        BuildError::Unsupported { expr, .. } | BuildError::UnsupportedType { expr, .. } | BuildError::NoConstBlockValue { expr, .. } | BuildError::NoConstFnInstance { expr, .. } => {
            body.and_then(|body| ranges(&body.expr_ranges, expr.0))
        }
        BuildError::UnsupportedPat { pat, .. } => body.and_then(|body| ranges(&body.pat_ranges, pat.0)),
        BuildError::NoAdt { adt }
        | BuildError::NoAbi { item: adt }
        | BuildError::NoStatic { item: adt, .. }
        | BuildError::NoConstValue { item: adt, .. }
        | BuildError::NoPatConstValue { item: adt, .. }
            if let Some(why) = program.left_out(adt) =>
        {
            return RunError::LeftOut { function, item: adt, path: program.item_named(adt), why: Box::from(why) };
        }
        BuildError::NoAdt { adt } => return RunError::NoAdt { function, adt, path: program.item_named(adt) },
        BuildError::NoStatic { item, .. } | BuildError::NoConstValue { item, .. } | BuildError::NoPatConstValue { item, .. } | BuildError::MissingRecord { item } => {
            return RunError::ItemRefused { function, error, item, path: program.item_named(item) };
        }
        BuildError::Projection { .. } if let Some(why) = program.last_projection() => {
            return RunError::Projection { function, why };
        }
        _ => None,
    };
    RunError::Build { function, error, range }
}

/// The instances one made entry declares: (tabled fn, name) rows over one arena, each once.
struct Reach<'a> {
    rows: AVec<'a, (ItemKey, u32, u32)>,
    text: AVec<'a, u8>,
    scratch: AVec<'a, u8>,
}

impl<'a> Reach<'a> {
    fn new_in(alloc: Alloc<'a>) -> Reach<'a> {
        Reach { rows: blaze_own::Vec::new_in(alloc), text: blaze_own::Vec::new_in(alloc), scratch: blaze_own::Vec::new_in(alloc) }
    }

    fn row(&self, n: usize) -> (ItemKey, &[u8]) {
        let (tabled, at, len) = self.rows[n];
        (tabled, &self.text[at as usize..(at + len) as usize])
    }

    /// Declares `callee` (tabled as `tabled`) and returns its symbol.
    fn declare(&mut self, tabled: ItemKey, callee: Instance, ground: &PortableTys<'_>, alloc: Alloc<'_>) -> Result<blaze_code::record::Symbol, crate::symbols::SymbolError> {
        self.scratch.clear();
        encode_instance(callee.item, callee.args, ground.view(), alloc, &mut self.scratch)?;
        let symbol = fn_symbol_of_name(&self.scratch);
        let seen = (0..self.rows.len()).any(|n| {
            let (t, bytes) = self.row(n);
            t == tabled && bytes == &self.scratch[..]
        });
        if !seen {
            let at = self.text.len() as u32;
            self.text.extend_from_slice(&self.scratch);
            self.rows.push((tabled, at, self.scratch.len() as u32));
        }
        Ok(symbol)
    }
}

/// How one reference of a built body is named.
enum Named1<'p> {
    Instance(blaze_code::record::Symbol),
    C(&'p str),
    /// The fn a trait's fn resolved to (the selected impl's) is tabled by no row yet: a lazy
    /// front end tables a body's callees as the body names them, and an impl's fn a call
    /// reaches only at ground arguments (`Future::poll` of an `.await`ed future) is named by
    /// no body. [`Step::Table`] has it checked; `.1` is the refusal if that tables no row.
    Untabled(ItemKey, RunError),
}

/// build_from's reference loop for one reference, by name: a closure's, a coroutine's or a
/// shim's instance tabled as its owner; a trait's fn as the selected impl's; an outside fn by
/// its C name. A reference with no instance is this instance's refusal (no stub on this path).
#[allow(clippy::too_many_arguments)]
fn name_ref<'p, P: FrontEnd>(
    program: &'p P,
    adts: &FrontAdts<'_, P>,
    ground: &mut PortableTys<'static>,
    alloc: Alloc<'static>,
    reach: &mut Reach<'_>,
    i: usize,
    at: List,
    called: Instance,
) -> Result<Named1<'p>, RunError> {
    let function = program.function(i);
    let name = || function.named();
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    // A fn item of a trait's method as an `Fn*` value: resolved first (fn_item_resolve.rs).
    let called = match crate::fn_item_resolve::resolved_shim_self(program, adts, i, called, ground) {
        Ok(c) => c,
        Err(error) => {
            let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
            return Err(RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error });
        }
    };
    // A callee whose name passes its cap (rustc's type-length limit) is this instance's
    // refusal, the callee shown by its head path and arguments (rule 19); the test's NOT RUN.
    let named = |e: crate::symbols::SymbolError, callee: Instance, ground: &PortableTys<'static>| match e {
        crate::symbols::SymbolError::TooLarge { nodes, bytes } => {
            callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NameTooLarge { nodes, bytes }, ground.view())
        }
        e => sym(e),
    };
    if let Some(c) = language_owner(program, adts, called, ground, i) {
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
    }
    // A fn item's (or a closure's) `Fn*` method whose item no row tables yet: the fn item was
    // passed as a zero-sized value, named by no body's callees (fn_shim.rs `untabled_owner`).
    // Checked, which tables its row, then this instance made again: the shim is declared then.
    if let Some(owner) = crate::fn_shim::untabled_owner(program, adts, called, ground) {
        let refusal = callee_refusal(program, name(), at, owner, crate::callee_shown::CalleeGap::NotTabled, ground.view());
        return Ok(Named1::Untabled(owner.item, refusal));
    }
    // `Clone::clone` at a fn item or fn pointer: the language's copy, no impl item to select
    // (clone_shim.rs `language_copy_clone`; t1800: proptest's derived `Clone for Config`
    // cloning its `fn(..)` field, 30 tests NOT RUN). Tabled at the trait's `clone` row.
    if let Some(clone) = crate::clone_shim::language_copy_clone(program, called, ground) {
        let Some(c) = program.fn_index(clone) else {
            let refusal = callee_refusal(program, name(), at, called, crate::callee_shown::CalleeGap::NotTabled, ground.view());
            return Ok(Named1::Untabled(clone, refusal));
        };
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
    }
    // A ctor is no trait's fn: it runs as itself, with no world asked (build_from's refs loop).
    // Known by its ADT's head (`ctor_of`), whether or not a row tables it yet: a lazy front end
    // tables a body's CALLEES, and a ctor used as a value (`let p: fn(..) -> S = S`) is none.
    let is_ctor = program.ctor_of(called.item).is_some();
    let selected = match is_ctor {
        true => Ok((called.item, called.args)),
        false => program.resolve(i, called.item, called.args, ground),
    };
    let (item, resolved) = match selected {
        Ok(r) => r,
        Err(error) => return Err(resolve_refusal(program, adts, ground, name(), called, error)),
    };
    let callee = Instance { item, args: resolved };
    // `at`: the arguments of the instance whose body makes this call (its `Self` first).
    let Some(c) = program.fn_index(callee.item) else {
        // Owed a row whatever names it: a ctor's ([`Step::Table`] makes it by the seam's
        // `head_row`), a selected impl's fn, and a fn the body names as a VALUE, which its
        // caller's check does not table as a callee (bstd `main!`'s `guest::start(table,
        // hosted)`: the local `hosted` passed as a fn item, hello world's stop of 2026-11-17
        // g080047). A fn a check of it tables no row for is refused when met again.
        let refusal = callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NotTabled, ground.view());
        return Ok(Named1::Untabled(callee.item, refusal));
    };
    let target = program.function(c);
    if target.kind == FnKind::Foreign {
        return Ok(Named1::C(target.symbol));
    }
    if bodied(program, adts.4.checked, c) {
        return reach.declare(target.key, callee, ground, alloc).map(Named1::Instance).map_err(|e| named(e, callee, &*ground));
    }
    Err(callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NoBody, ground.view()))
}

/// Whether fn row `c` is made as an instance of its own: its body is held now, or this run
/// checked it (a made body `done` dropped), or it is owed one: a row no `check_one` of this
/// run was asked of yet, neither an outside fn nor an intrinsic. A front end that tables every
/// body up front holds each now; the lazy program (front2/program/lazy) tables a callee's row
/// from its head and makes the body when the walk reaches the callee's own instance, whose
/// make checks it ([`MakerState::ensure`]) and refuses by name a fn with no body to check.
fn bodied<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>], c: usize) -> bool {
    let f = program.function(c);
    if f.body.is_some() || checked.get(c).is_some_and(Option::is_some) {
        return true;
    }
    f.kind != FnKind::Foreign && program.intrinsic(f.key).is_none()
}

/// build_from's vtable loop for one vtable the body names, by name: the method words and the
/// drop glue of it and of each table above it (by a work list, no recursion) declared as the
/// instances they reach. The tables' bytes are data, made by their own record (D106 item 6).
/// A word whose fn no row tables yet (the drop glue `drop_in_place`, an impl's method no body
/// called: a lazy front end tables a body's CALLEES, and a vtable's words are no body's) goes
/// to `untabled`, as an untabled callee of a call does ([`Named1::Untabled`]): checked, then
/// the instance made again. Each table's DATUM goes onto `data` by its symbol, each once (the
/// entry carries it: code_records.rs `VtableDatum`), when every word of it is named.
#[allow(clippy::too_many_arguments)]
fn vtable_rows<P: FrontEnd>(
    program: &P,
    adts: &FrontAdts<'_, P>,
    ground: &mut PortableTys<'static>,
    layouts: &mut Layouts<Alloc<'static>>,
    alloc: Alloc<'static>,
    reach: &mut Reach<'_>,
    i: usize,
    at: List,
    want: VtableRef,
    untabled: &mut Vec<(ItemKey, RunError), Global>,
    data: &mut AVec<'_, (blaze_code::record::Symbol, AVec<'static, u8>)>,
) -> Result<(), RunError> {
    let function = program.function(i);
    let name = || function.named();
    let fail = |why| RunError::Vtable { function: name(), principal: want.principal, why };
    let full = || fail(VtableFail::Build(BuildError::Malformed { what: "the ground table is full" }));
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    // A method word's instance whose name passes its cap: this instance's refusal, as a
    // call's is (`name_ref`).
    let named = |e: crate::symbols::SymbolError, callee: Instance, ground: &PortableTys<'static>| match e {
        crate::symbols::SymbolError::TooLarge { nodes, bytes } => {
            callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NameTooLarge { nodes, bytes }, ground.view())
        }
        e => sym(e),
    };
    let mut todo: AVec<'_, VtableRef> = blaze_own::Vec::new_in(alloc);
    let mut seen: AVec<'_, VtableRef> = blaze_own::Vec::new_in(alloc);
    todo.push(want);
    while let Some(t) = todo.pop() {
        if seen.contains(&t) {
            continue;
        }
        seen.push(t);
        let shape = Shape::of(t.principal, |k| program.object_trait(k), Global).map_err(|e| fail(VtableFail::Shape(e)))?;
        for w in shape.words.iter() {
            if let Word::Super(s) = *w {
                todo.push(VtableRef { concrete: t.concrete, principal: s, args: List::EMPTY });
            }
        }
        let id = {
            let mut cx = LayoutCx { ground: &mut *ground, facts: adts, scratch: alloc };
            layouts.of(t.concrete, &mut cx).map_err(|e| fail(VtableFail::Layout(e)))?
        };
        let (size, align) = (layouts.get(id).size, layouts.get(id).align_bytes());
        let glue = match program.fn_lang(FnLang::DropInPlace) {
            Some(item) => Some(Instance { item, args: ground.intern_list(&[t.concrete]).map_err(|_| full())? }),
            None => match program.lang_item(LangItem::Drop).map_err(RunError::Lang)? {
                None => None,
                Some(_) => return Err(fail(VtableFail::NoDropGlue)),
            },
        };
        let glue_item = glue.map(|g| g.item);
        let mut targs: AVec<'_, PTy> = blaze_own::Vec::new_in(alloc);
        targs.extend_from_slice(ground.view().list(t.args).map_err(|_| full())?);
        let table = Vtable::build(&shape, t.concrete, &targs, size, align, glue, &mut *ground, Global).map_err(|e| fail(VtableFail::Build(e)))?;
        // The table's datum (code_records.rs `VtableDatum`): its bytes as built (the header's
        // size and alignment, every address word 0), each address word a target by symbol.
        let mut datum = blaze_code::datum::Datum::new_in(blaze_code::datum::Place::ReadOnly, 3, alloc);
        datum.bytes.extend_from_slice(&table.bytes);
        // False once a word's fn is untabled: the instance is made again ([`Step::Table`]), and
        // this table's datum with it.
        let mut whole = true;
        for &(off, to) in table.relocs.iter() {
            let f = match to {
                VtableTarget::Fn(f) => f,
                VtableTarget::Super(s) => {
                    let symbol = vtable_symbol(VtableRef { concrete: t.concrete, principal: s, args: List::EMPTY }, ground.view(), alloc).map_err(sym)?;
                    let target = datum.target(blaze_code::record::Target { space: blaze_code::record::Space::Vtable, symbol });
                    datum.words.push(blaze_code::record::Word { at: off, target, addend: 0 });
                    continue;
                }
            };
            let mut word = |symbol: blaze_code::record::Symbol| {
                let target = datum.target(blaze_code::record::Target { space: blaze_code::record::Space::Func, symbol });
                datum.words.push(blaze_code::record::Word { at: off, target, addend: 0 });
            };
            // As a call's callee (`name_ref` above): a fn item of a trait's method resolved
            // to the body that runs, then what the language gives (`language_owner`).
            let f = match crate::fn_item_resolve::resolved_shim_self(program, adts, i, f, ground) {
                Ok(f) => f,
                Err(error) => {
                    let at = crate::callee_shown::self_shown(program, ground.view(), f.args);
                    return Err(RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), at, error });
                }
            };
            if let Some(c) = language_owner(program, adts, f, ground, i) {
                word(reach.declare(program.function(c).key, f, ground, alloc).map_err(|e| named(e, f, &*ground))?);
                continue;
            }
            // A `dyn Fn*` made from a fn item (or a closure) whose item no row tables yet: as
            // `name_ref`'s, checked and the instance made again (fn_shim.rs `untabled_owner`).
            if let Some(owner) = crate::fn_shim::untabled_owner(program, adts, f, ground) {
                if !untabled.iter().any(|(k, _)| *k == owner.item) {
                    let refusal = callee_refusal(program, name(), at, owner, crate::callee_shown::CalleeGap::VtableWord, ground.view());
                    untabled.push((owner.item, refusal));
                }
                whole = false;
                continue;
            }
            let (item, args) = match Some(f.item) == glue_item {
                true => (f.item, f.args),
                false => match program.resolve(i, f.item, f.args, &mut *ground) {
                    Ok(r) => r,
                    Err(error) => return Err(resolve_refusal(program, adts, ground, name(), f, error)),
                },
            };
            let refusal = || callee_refusal(program, name(), at, Instance { item, args }, crate::callee_shown::CalleeGap::VtableWord, ground.view());
            let Some(c) = program.fn_index(item) else {
                if !untabled.iter().any(|(k, _)| *k == item) {
                    untabled.push((item, refusal()));
                }
                whole = false;
                continue;
            };
            if !bodied(program, adts.4.checked, c) {
                return Err(refusal());
            }
            word(reach.declare(program.function(c).key, Instance { item, args }, ground, alloc).map_err(|e| named(e, Instance { item, args }, &*ground))?);
        }
        if whole {
            let symbol = vtable_symbol(t, ground.view(), alloc).map_err(sym)?;
            let sealed = datum.seal().map_err(|why| RunError::CodeRecord { function: name(), error: CodeRecordError::VtableDatum(why) })?;
            let mut bytes: AVec<'static, u8> = blaze_own::Vec::new_in(alloc);
            sealed.encode(&mut bytes);
            if !data.iter().any(|(s, _)| *s == symbol) {
                data.push((symbol, bytes));
            }
        }
    }
    Ok(())
}

/// What the command hands the [`Maker`]: every crate's names READ FROM RECORDS, nothing solved
/// in this command. The same four things `blaze check` hands its world (blaze-cli
/// body_types.rs `checker`: a `StoreLoad` over each crate's `CrateScope` record, a `Log` over
/// the crates' names, `Records::new(load.keys(g), load, log, frame, g)`, and
/// `ProjectWorld::from_records(records, frame, owner.lang, g)` with `set_body_sources`), so the
/// command makes them ONE way for both. All borrowed for `'n`, the command's life: the maker
/// holds no input of its own, so nothing in it borrows itself.
#[derive(Clone, Copy)]
pub struct NamesRecords<'n> {
    /// Each crate's names and items records, the program's own at index 0, opened on the first
    /// question about each (`Records::new`: nothing is read when it is made).
    pub records: &'n crate::front2::source::Records<'n>,
    /// The command's frame: what a body's lowering keeps for the command (`Front2::frame`).
    pub frame: &'n blaze_mem::Frame,
    /// The crates that can declare a lang item, by index among `records`' crates: the
    /// program's own (0), core, alloc, std, and bstd under a blaze edition
    /// (`Front2::from_records` reads only their lang rows).
    pub lang_crates: &'n [usize],
    /// The sources a body's macro calls expand with (environment, texts, proc-macro runner:
    /// front2 body_host.rs); `None`: every body `env!`, include and proc macro is refused by
    /// name.
    pub body_sources: Option<&'n dyn crate::front2::body_host::BodySources>,
}

/// The one lazy program the maker makes: over the names records.
pub type RecordsProgram<'n> = crate::front2::program::lazy::LazyProgram<'n, &'n crate::front2::source::Records<'n>>;

/// An instance for a person ([`Maker::instance_shown`]): its item by the one printer, and its
/// ground arguments as Rust spells them (`None`: it has none).
pub struct InstanceShown {
    pub item: crate::front2::item_shown::ItemShown,
    pub args: Option<Box<str>>,
}

/// THE RECORD WALK'S MAKER, held by the parent for the whole program (D124): one owned session
/// over the names READ FROM RECORDS. [`Maker::new`] resolves, reads and checks nothing; the
/// first [`Make::make`] constructs the lazy program over the records (no `resolve_crates`
/// anywhere under it), and every make after it types only the bodies its instance needs
/// (`check_one`). A run that finds every record constructs no program and reads no record.
///
/// What the records could not answer is in their log (front2 source.rs): after each make the
/// log is read, and a make over a log that is not empty is refused
/// ([`LazyRefusal::RecordsVoid`]), as is every make after it (the session is void).
///
/// Two type parameters, each a seam chosen by type (rule 78): `G`, the proof a make is handed
/// (body_ids.rs `Grant`: blaze-cli's run gate's `Runnable` in production, `Fixture` in this
/// crate's walks), and `T`, where a body's checked columns come from ([`Typing`]).
pub struct Maker<'n, G: Grant = Fixture, T: Typing<RecordsProgram<'n>> = SecondCheck> {
    /// Where each body's checked columns come from.
    typing: T,
    /// The proof the maker's makes are handed: a type only.
    grant: core::marker::PhantomData<fn(&G)>,
    names: NamesRecords<'n>,
    entry: Option<ItemKey>,
    /// Where its program starts: the front's roots' say (run/special.rs `MakerStart`).
    start: crate::run::special::MakerStart,
    made: Option<(RecordsProgram<'n>, MakerState)>,
    isa: Isa,
    overflow: OverflowChecks,
    ub: blaze_build::UbChecks,
    panic: blaze_build::Panic,
    alloc: Alloc<'static>,
    checks: CheckCount,
}

impl<'n> Maker<'n, Fixture, SecondCheck> {
    /// The maker over `names`. `entry`: the checker key of the fn the process starts at, as
    /// `LazyProgram::from_records` takes it (nothing lowers it until a make asks). `builder`:
    /// the records' builder; its ISA, overflow setting and panic strategy are the makes' (rule
    /// 21: one source, nothing to compare). `alloc`: the `'static` heap the lazy program's
    /// tables live in (`Store::new`, `MakerState`). Cheap: reads, resolves and checks nothing.
    ///
    /// Its program starts by THE start rule (`MakerStart::ByRule`): a program's run, for this
    /// crate's walks (`Fixture`), each body checked in the make. A parent whose roots name
    /// another start takes [`Maker::of_roots`]; a walk reading another typing,
    /// [`Maker::typed`].
    pub fn new(names: NamesRecords<'n>, entry: Option<ItemKey>, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n> {
        Maker::starting(names, entry, crate::run::special::MakerStart::ByRule, SecondCheck, builder, alloc)
    }
}

impl<'n, T: Typing<RecordsProgram<'n>>> Maker<'n, Fixture, T> {
    /// [`Maker::new`] with the body's checked columns from `typing`: the oracle's two sides.
    pub fn typed(names: NamesRecords<'n>, entry: Option<ItemKey>, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, Fixture, T> {
        Maker::starting(names, entry, crate::run::special::MakerStart::ByRule, typing, builder, alloc)
    }
}

impl<'n, G: Grant, T: Typing<RecordsProgram<'n>>> Maker<'n, G, T> {
    /// The maker for the roots a front handed (run/special.rs `Roots`): `entry` as [`Maker::new`]
    /// takes it, and the program's start the roots' own (`Roots::maker_start`): `blaze run`'s
    /// roots start by the rule, a test's at its fn. The one way the lazy parent makes a maker.
    /// `typing`: where each body's checked columns come from (production: [`SecondCheck`]
    /// until the check keeps its typed record; then [`FromRecord`]).
    pub fn of_roots(names: NamesRecords<'n>, entry: Option<ItemKey>, roots: &crate::run::special::Roots<'_>, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, G, T> {
        Self::starting(names, entry, roots.maker_start, typing, builder, alloc)
    }

    fn starting(names: NamesRecords<'n>, entry: Option<ItemKey>, start: crate::run::special::MakerStart, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, G, T> {
        Maker {
            typing,
            grant: core::marker::PhantomData,
            names,
            entry,
            start,
            made: None,
            isa: builder.isa,
            overflow: builder.settings.overflow_checks,
            // `ub_checks()` follows the key's debug assertions, as rustc's `ub_checks` cfg
            // follows `-C debug-assertions`: in the key, so never stale (rule 21).
            ub: blaze_build::UbChecks::of_debug_assertions(builder.settings.debug_assertions),
            panic: crate::panic_strategy::of_strategy(builder.settings.panic),
            alloc,
            checks: CheckCount::default(),
        }
    }

    /// Whether an instance made in this session touched a thread-local. A FOUND record's use of
    /// one is said by bzcode03's declared rows (kinds 4 and 5); until that form lands the
    /// caller makes the getter's and ender's records when this is true or its own records say so.
    pub fn made_thread_locals(&self) -> bool {
        self.made.as_ref().is_some_and(|(_, s)| s.work.thread_locals)
    }

    /// The checks the makes cost so far.
    pub fn checks(&self) -> CheckCount {
        self.checks
    }

    pub fn lowering_counts(&self) -> LoweringCounts {
        self.program().map_or_else(LoweringCounts::default, |program| program.lowering_counts())
    }

    /// The lazy program, once a miss made it.
    pub fn program(&self) -> Option<&RecordsProgram<'n>> {
        self.made.as_ref().map(|(p, _)| p)
    }

    /// Instance `name` (symbols.rs `encode_instance` bytes) of fn `tabled`, for a person
    /// (rule 19: a crash names the fn and, since generics matter for a miscompile, its ground
    /// arguments): the item its name holds by the one printer (front2 item_shown.rs
    /// `ItemShown`: a path, or where a path-less item is written) and its arguments as Rust
    /// spells them (callee_shown.rs `args_shown`: a closure as `<closure N of owner>`). `None`
    /// before a make built the program (a run whose every record was found): the caller then
    /// names the item from the records alone. Read only on a crash's path.
    pub fn instance_shown(&self, tabled: ItemKey, name: &[u8]) -> Option<InstanceShown> {
        let program = self.program()?;
        let mut ground = PortableTys::new_in(self.alloc);
        let (item, args) = match decode_instance(name, &mut ground, self.alloc) {
            Ok((item, args)) => {
                let view = ground.view();
                let args = match view.list(args) {
                    Ok([]) => None,
                    _ => Some(crate::callee_shown::args_shown(view, args, &|k| program.item_named(k))),
                };
                (item, args)
            }
            Err(_) => (tabled, None),
        };
        Some(InstanceShown { item: program.item_shown(item), args })
    }

    /// Byte `site` of item `item`'s text (a code record's trap row, item-relative) for a
    /// person: its file, line and column today (lazy fill.rs `site_shown`). `None` before a
    /// make built the program, or when the item's text is in no file. Read only on a crash's
    /// path.
    pub fn site_shown(&self, item: ItemKey, site: u32) -> Option<(&'n str, u32, u32)> {
        self.program()?.site_shown(item, site)
    }

    /// The refusal of a make over records whose log is not empty, naming its first error.
    fn void(&self, tabled: ItemKey) -> Option<RunError> {
        let log = self.names.records.log();
        let errors = log.errors();
        let first = errors.first()?;
        let first = std::format!("{first}").into_boxed_str();
        Some(RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled, first, kept: errors.len(), dropped: log.dropped() })))
    }

    /// The program made on the first call, then the one instance.
    fn make_over(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.construct(tabled)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `Maker::make`)" });
        };
        make_instance(program, &mut self.typing, state, &mut self.checks, tabled, name, entry)
    }

    /// The program made on the first call, then static `item`'s value made its entry
    /// (code_records.rs `statics`): a static's datum, or (`template`) a thread-local's template.
    fn static_over(&mut self, item: ItemKey, template: bool, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.construct(item)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: item, why: "the maker's program was not made (make_one.rs `Maker::make_static`)" });
        };
        let kind = program.make_static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
        let value = program.static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
        let made = match state.static_entry(program, value, kind, template) {
            Ok(made) => made,
            Err(error) => return Err(RunError::StaticRecord { static_: program.static_shown(item), error }),
        };
        entry.extend_from_slice(&made);
        Ok(())
    }

    /// The lazy program over the records, made on the first make of either kind (`first`: the
    /// item that make is of).
    fn construct(&mut self, tabled: ItemKey) -> Result<(), RunError> {
        if self.made.is_none() {
            let program = program_of(self.names, self.entry, self.start, self.alloc)?;
            self.checks.constructed += 1;
            // A lazy program holds no checked body when made: 0, counted as the claim reads it.
            self.checks.by_construction += (0..program.fn_count()).filter(|&c| program.function(c).body.is_some()).count();
            let state = MakerState::new(&program, tabled, self.isa, self.overflow, self.ub, self.panic, self.alloc)?;
            self.made = Some((program, state));
        }
        Ok(())
    }
}

/// The lazy program over `names`, its body sources set and its start decided: the one way a
/// maker's program is made ([`Maker`]'s first make, and the test double [`FromRecord::rechecked`]).
fn program_of<'n>(names: NamesRecords<'n>, entry: Option<ItemKey>, start: crate::run::special::MakerStart, alloc: Alloc<'static>) -> Result<RecordsProgram<'n>, RunError> {
    let NamesRecords { records, frame, lang_crates, body_sources } = names;
    let mut program = RecordsProgram::from_records(records, frame, lang_crates, entry, alloc).map_err(RunError::Front2)?;
    if let Some(sources) = body_sources {
        program.set_body_sources(sources);
    }
    // The start the front's roots named (run/special.rs `MakerStart`): by the one rule
    // run/special.rs `roots` takes (the seam's `rust_start`), a refusal here the parent's own
    // roots refusal met again, never passed over; or a test fn, called directly.
    program.decide_start(start).map_err(RunError::Roots)?;
    Ok(program)
}

/// The typed records' test double over a maker's names (lazy typed.rs `Rechecked`).
pub type RecheckedRecords<'n> = crate::front2::program::lazy::typed::Rechecked<'n, &'n crate::front2::source::Records<'n>>;

impl<'n> FromRecord<RecheckedRecords<'n>> {
    /// THE TEST DOUBLE as a typing: each body's checked columns made by today's second check in
    /// a lazy program of its own over `names` (the same records, start and entry as the maker
    /// it serves; [`Maker::new`]'s start rule), handed over as a typed record would hand them.
    pub fn rechecked(names: NamesRecords<'n>, entry: Option<ItemKey>, alloc: Alloc<'static>) -> Result<FromRecord<RecheckedRecords<'n>>, RunError> {
        let program = program_of(names, entry, crate::run::special::MakerStart::ByRule, alloc)?;
        Ok(FromRecord(crate::front2::program::lazy::typed::Rechecked::new(program, alloc)))
    }
}

/// THE ORACLE of the typing seam (D163 part 2): every make made twice, by `checking` (the body
/// checked in the make, [`SecondCheck`]) and by `reading` (its checked columns from the typed
/// record, [`FromRecord`]), and the two entries compared byte for byte: an entry is the
/// instance's code, its declared names and its vtables (code_records.rs `make_entry`), so equal
/// entries are equal LIR emitted the same way. The first difference refuses the make by its
/// fn's key; `compared` counts the entries held equal. The walk keeps `checking`'s entry.
pub struct Twin<A, B> {
    pub checking: A,
    pub reading: B,
    /// Entries compared and found equal.
    pub compared: usize,
    scratch: AVec<'static, u8>,
}

impl<A, B> Twin<A, B> {
    pub fn new(checking: A, reading: B, alloc: Alloc<'static>) -> Twin<A, B> {
        Twin { checking, reading, compared: 0, scratch: blaze_own::Vec::new_in(alloc) }
    }

    /// `checking`'s entry is in `entry` from `at`; `reading`'s in the scratch.
    fn same(&mut self, key: ItemKey, entry: &AVec<'_, u8>, at: usize) -> Result<(), RunError> {
        if entry[at..] != self.scratch[..] {
            return Err(RunError::OneBody { key, why: "the typing oracle: the entry made from the typed record differs from the entry made by checking the body again (make_one.rs `Twin`)" });
        }
        self.compared += 1;
        Ok(())
    }
}

impl<A: Make<Error = RunError, Grant = Fixture>, B: Make<Error = RunError, Grant = Fixture>> Make for Twin<A, B> {
    type Error = RunError;
    type Grant = Fixture;

    fn make(&mut self, grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make(grant, tabled, name, entry)?;
        self.scratch.clear();
        self.reading.make(grant, tabled, name, &mut self.scratch)?;
        self.same(tabled, entry, at)
    }

    fn make_static(&mut self, grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make_static(grant, item, entry)?;
        self.scratch.clear();
        self.reading.make_static(grant, item, &mut self.scratch)?;
        self.same(item, entry, at)
    }

    fn make_thread_local(&mut self, grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make_thread_local(grant, item, entry)?;
        self.scratch.clear();
        self.reading.make_thread_local(grant, item, &mut self.scratch)?;
        self.same(item, entry, at)
    }
}

/// A make is handed the proof `G` (body_ids.rs `Make`): the maker reads nothing of it, its
/// presence at the call is the obligation (rule 76).
impl<'n, G: Grant, T: Typing<RecordsProgram<'n>>> Make for Maker<'n, G, T> {
    type Error = RunError;
    type Grant = G;

    fn make(&mut self, _grant: &G, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        let made = self.make_over(tabled, name, entry);
        // The log first: a refusal or an entry made over an unanswered question is void, and
        // the records' error is its cause.
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        made
    }

    fn make_static(&mut self, _grant: &G, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        let made = self.static_over(item, false, entry);
        // As `make`: an entry made over an unanswered question is void.
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        made
    }

    fn make_thread_local(&mut self, _grant: &G, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        let made = self.static_over(item, true, entry);
        // As `make`: an entry made over an unanswered question is void.
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        made
    }
}

/// Why the record walk over `blaze run`'s records stopped.
#[derive(Debug)]
pub enum RecordWalkError {
    /// The entry's name could not be made.
    Entry(crate::symbols::SymbolError),
    /// The walk, the store or one make refused.
    Walk(WalkError<RunError>),
}

impl core::fmt::Display for RecordWalkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordWalkError::Entry(e) => write!(f, "the record walk's entry: {e}"),
            RecordWalkError::Walk(e) => e.fmt(f),
        }
    }
}

impl core::error::Error for RecordWalkError {}

/// The most instances one record walk reaches (rule 18).
pub const MOST_WALKED: usize = 1 << 22;

#[cfg(test)]
#[path = "../../tests/front_equal/lazy_lowering.rs"]
mod lazy_lowering;

/// `blaze run`'s code records by D106: from `entry` (the program's `main`, by its checker key:
/// the command reads it from crate 0's names record) every instance reached is FOUND in `store`
/// by its key from text (`ids`) or made by `maker` and put. Returns the walk (its rows: symbol,
/// key, tabled fn, name, how) for the launcher, and the counts line.
pub fn walk_records<M: Make<Error = RunError, Grant = Fixture>>(
    entry: ItemKey,
    ids: &dyn BodyIds,
    store: &mut dyn CodeStore,
    builder: &Builder,
    producer: &ProducerId<CodeBuilder>,
    maker: &mut M,
    alloc: Alloc<'static>,
) -> Result<(Walk<'static>, Counts), RecordWalkError> {
    let mut name: AVec<'_, u8> = blaze_own::Vec::new_in(alloc);
    let ground = PortableTys::new_in(alloc);
    encode_instance(entry, List::EMPTY, ground.view(), alloc, &mut name).map_err(RecordWalkError::Entry)?;
    let mut walk = Walk::new_in(alloc);
    walk.run((entry, &name), ids, store, builder, producer, maker, &EntryForm, MOST_WALKED).map_err(RecordWalkError::Walk)?;
    let counts = walk.counts();
    Ok((walk, counts))
}
