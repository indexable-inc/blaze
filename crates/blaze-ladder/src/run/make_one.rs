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
//!   run: the instance's tabled fn, and each fn its NAME holds (`body_ids::each_item`: a
//!   closure's or coroutine's owner, whose facts the layouts read), and each async block's owner
//!   a coroutine demand names. Over-counts by design: a fn ITEM type in the arguments is checked
//!   too (one name cannot tell a fn item's key from a closure owner's).
//! - `eval_consts` (every `const` and `static` of the program): not run. The record walk takes a
//!   front end that evaluates each constant at its arguments (`evaluates_at_args`): the per-item
//!   pull `const_at::needs_of` + `const_at::pull` for THIS instance. The old front is refused by
//!   name ([`RunError::OneBody`]).
//! - `static_image` (every static's bytes): not run. A record names a static, thread-local or
//!   vtable by symbol; each one's data is its own record (D106 item 6, bzcode03's kinds 1-3).
//! - `ClosureTable::of` and `CoroutineTable::of` (every checked body): re-made over the bodies
//!   checked SO FAR, only when a newly checked body has a closure or an async block
//!   ([`MakerState::ensure`]). The end form is one row added per body (a `ClosureTable::add`
//!   beside `of`), with piece Y.
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

use crate::body_ids::{BodyIds, Counts, EntryForm, Make, Walk, WalkError, each_item};
use crate::code_records::{Builder, Callee, CodeRecordError, CodeStore, Declared, InstanceNames, make_entry};
use crate::front2::const_at::{ConstTable, Needs};
use crate::front2::coroutines::{CoroutineTable, Coroutines};
use crate::front2::program::lazy::LazyRefusal;
use super::lay_states::{Lay, lay_and_build, roots_of_instance, unchecked_owner};
use crate::symbols::{decode_instance, encode_instance, fn_symbol_of_name, static_symbol, tls_symbol, vtable_symbol};

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
}

impl CheckCount {
    /// Every check the makes cost.
    pub fn all(&self) -> usize {
        self.check_one + self.by_construction
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
    /// The state for `program`, before its first instance. Refuses a front end that evaluates
    /// constants for the whole program only (`evaluates_at_args` false), naming `first`.
    pub fn new<P: FrontEnd>(program: &P, first: ItemKey, isa: Isa, overflow: OverflowChecks, panic: blaze_build::Panic, alloc: Alloc<'static>) -> Result<MakerState, RunError> {
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
    fn ensure<P: FrontEnd>(&mut self, program: &mut P, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        /// The most closure owners one ensure follows (rule 18).
        const MOST_OWNERS: usize = 4096;
        let first = self.ensure_one(program, key, checks)?;
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
            self.ensure_one(program, owner, checks)?;
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
    fn ensure_one<P: FrontEnd>(&mut self, program: &mut P, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        let slot = match self.asked.binary_search_by_key(&key, |r| r.0) {
            Ok(n) => return Ok(self.asked[n].1),
            Err(n) => n,
        };
        let got = program.check_one(key)?;
        let at = match got {
            None => None,
            Some((i, done)) => {
                checks.check_one += 1;
                while self.checked.len() < program.fn_count().max(i + 1) {
                    self.checked.push(None);
                }
                let mut refresh = done.closures.len() != 0;
                if !refresh && let Some((body, _)) = program.function(i).body {
                    let view = body.view(program.types());
                    refresh = blaze_build::coroutines_in(&view).into_iter().next().is_some();
                }
                self.checked[i] = Some(done);
                if refresh {
                    let tables = self.tables.as_mut().ok_or_else(|| tables_gone(key))?;
                    tables.closures = ClosureTable::of(&*program, &self.checked, self.work.alloc);
                    tables.coroutines.table = CoroutineTable::of(&*program, &self.checked, self.work.alloc);
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

/// Makes ONE instance: `name` (symbols.rs `encode_instance` bytes), tabled as fn `tabled`;
/// appends its entry (code_records.rs `make_entry`) to `entry`. The one generic step of D106:
/// every front end behind the seam is made through it (rule 78).
pub fn make_instance<P: FrontEnd>(
    program: &mut P,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    name: &[u8],
    entry: &mut AVec<'_, u8>,
) -> Result<(), RunError> {
    let alloc = state.work.alloc;
    // The bodies this ONE instance reads: its fn's, and each owner its name holds.
    let Some(i) = state.ensure(program, tabled, checks)? else {
        return Err(RunError::OneBody { key: tabled, why: "the record walk reached an instance of a fn the front end holds no checked body for" });
    };
    let mut items: AVec<'_, ItemKey> = blaze_own::Vec::new_in(alloc);
    each_item(name, |item| items.push(item)).map_err(|_| RunError::OneBody { key: tabled, why: "an instance name that does not list its items (body_ids.rs `each_item`)" })?;
    for &item in items.iter() {
        if item != tabled && program.fn_index(item).is_some_and(|c| program.function(c).body.is_some()) {
            state.ensure(program, item, checks)?;
        }
    }
    let (item, args) = decode_instance(name, &mut state.work.ground, alloc).map_err(|e| RunError::CodeRecord { function: program.function(i).named(), error: CodeRecordError::Symbol(e) })?;
    let instance = Instance { item, args };
    // The constants this instance reads at its ground arguments, pulled before the build
    // (front2/const_at.rs): a plain instance's own body only, as in build_from.
    pull_constants(program, state, i, instance)?;
    // The build, its coroutine states laid out from types first (lay_states.rs); a lay that
    // meets an owner fn not checked yet has it checked and the instance made again (no LIR
    // was built: types only were read); bounded (rule 18).
    let mut owners = 0usize;
    loop {
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, panic: work.panic, alloc: work.alloc };
        let step = build_one(&*program, &bodies, work, i, instance, entry);
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        match step? {
            Step::Entry => break,
            Step::Check(owner) => {
                owners += 1;
                if owners > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "an instance whose coroutine states needed more than 4096 owner fns checked" };
                    return Err(RunError::Build { function: program.function(i).named(), error, range: None });
                }
                if state.ensure(program, owner, checks)?.is_none() {
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
                    state.ensure(program, key, checks)?;
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

/// The per-item constant pull of build_from's step (`const_at::needs_of`, `const_at::pull`).
fn pull_constants<P: FrontEnd>(program: &mut P, state: &mut MakerState, i: usize, instance: Instance) -> Result<(), RunError> {
    let MakerState { checked, tables, work, .. } = &mut *state;
    work.needs.clear();
    let function = program.function(i);
    if instance.item != function.key {
        return Ok(());
    }
    let (Some((body, _)), Some(done)) = (function.body, checked.get(i).and_then(Option::as_ref)) else {
        return Err(callee_refusal(&*program, function.named(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view()));
    };
    let malformed = |_| RunError::Build { function: function.named(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    args.extend_from_slice(work.ground.view().list(instance.args).map_err(malformed)?);
    let t = tables.take().ok_or_else(|| tables_gone(function.key))?;
    let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, panic: work.panic, alloc: work.alloc };
    let view = body.view(program.types());
    let facts = FrontAdts(&*program, i, &[], &work.langs, &bodies);
    let found = crate::front2::const_at::needs_of(&view, done, &args, &mut work.ground, &facts, work.alloc, &mut work.needs);
    *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
    found.map_err(|error| RunError::Build { function: function.named(), error, range: None })?;
    if work.needs.is_empty() {
        return Ok(());
    }
    let named = function.named();
    let table = tables.as_mut().ok_or_else(|| tables_gone(instance.item))?;
    if let Err(refusal) = crate::front2::const_at::pull(&mut *program, &work.needs, work.ground.view(), &mut table.const_at) {
        let item = refusal.need.item();
        let path = program.item_named(item);
        return Err(RunError::ConstAt { function: named, refusal, path });
    }
    Ok(())
}

/// build_from's step for one instance, from "A closure's `Fn*` method instance" to
/// `make_entry`, over one borrow of the bodies.
fn build_one<P: FrontEnd>(program: &P, bodies: &Bodies<'_>, work: &mut Work, i: usize, instance: Instance, entry: &mut AVec<'_, u8>) -> Result<Step, RunError> {
    let types = program.types();
    let function = program.function(i);
    let name = || function.named();
    let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(i).and_then(Option::as_ref)) else {
        return Err(callee_refusal(program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view()));
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
    let view = body.view(types);
    let own = if from_coroutine.is_some() {
        None
    } else {
        let mut subst = Subst { args: &args, ground: &mut work.ground, scratch: work.alloc };
        let made = if let Some(s) = shim {
            match s.item {
                Some(_) => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, Some(&program.sig(i)), &mut subst, &adts, Global),
                None => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, None, &mut subst, &adts, Global),
            }
        } else if let Some((of, which)) = closure {
            blaze_build::build_closure(&view, done, of.ordinal, which, &program.sig(i), &mut subst, &adts, Global)
        } else if Some(function.key) == program.fn_lang(FnLang::DropInPlace) {
            blaze_build::build_drop_glue(&view, done, &program.sig(i), &mut subst, &adts, Global)
        } else {
            blaze_build::build(&view, done, &program.sig(i), &mut subst, &adts, Global)
        };
        Some(made.map_err(|error| build_refusal(program, name(), body, error))?)
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
    // Every untabled callee of this build at once: checked, then the instance made again.
    if !untabled.is_empty() {
        return Ok(Step::Table(untabled));
    }
    for &want in built.vtables.iter() {
        vtable_rows(program, &adts, &mut work.ground, &mut work.layouts, work.alloc, &mut reach, i, instance.args, want)?;
    }
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    let mut statics: AVec<'_, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(built.statics.len(), work.alloc);
    statics.extend(built.statics.iter().map(|&k| static_symbol(k)));
    let mut tls_names: AVec<'_, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(built.thread_locals.len(), work.alloc);
    tls_names.extend(built.thread_locals.iter().map(|&k| tls_symbol(k)));
    let mut vtables: AVec<'_, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(built.vtables.len(), work.alloc);
    for &table in built.vtables.iter() {
        vtables.push(vtable_symbol(table, work.ground.view(), work.alloc).map_err(sym)?);
    }
    // The soft-float routines after the instance's own references: the emitter's one table
    // (front2/builtins_shim.rs); `make_entry` declares those it says the record calls.
    let routines = work.routines.bind(&mut funcs).map_err(|error| RunError::CodeRecord { function: name(), error })?;
    let names = InstanceNames { funcs: &funcs, statics: &statics, vtables: &vtables, tls: &tls_names };
    let mut declared: AVec<'_, Declared<'_>> = blaze_own::Vec::with_capacity_in(reach.rows.len(), work.alloc);
    for n in 0..reach.rows.len() {
        let (tabled, name_bytes) = reach.row(n);
        declared.push(Declared { tabled, name: name_bytes });
    }
    let made = make_entry(&built.func, &names, routines, &mut declared, work.isa, work.alloc).map_err(|error| RunError::CodeRecord { function: name(), error })?;
    entry.extend_from_slice(&made);
    // The coroutine's part leaves the stash with its entry made, not before.
    if let Some((at, part)) = from_coroutine {
        work.stash[at].parts[part] = None;
    }
    Ok(Step::Entry)
}

/// build_from's mapping of a build error to the run's refusal, by the body's ranges.
fn build_refusal<P: FrontEnd>(program: &P, function: Named, body: &blaze_lower::typeck::TypeckBody<'static>, error: BuildError) -> RunError {
    let ranges = |all: &[blaze_typeck::Range], i: u32| all.get(i as usize).copied();
    let range = match error {
        BuildError::Unsupported { expr, .. } | BuildError::UnsupportedType { expr, .. } | BuildError::NoConstBlockValue { expr, .. } => ranges(&body.expr_ranges, expr.0),
        BuildError::UnsupportedPat { pat, .. } => ranges(&body.pat_ranges, pat.0),
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
            let at = crate::callee_shown::self_form(ground.view(), called.args);
            return Err(RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error });
        }
    };
    let tabled_as = crate::front2::coroutines::owner_of(program, adts, called, ground)
        .or_else(|| closure_owner(program, adts, called, ground))
        .or_else(|| crate::fn_shim::fn_shim_owner(program, adts, called, ground, i));
    if let Some(c) = tabled_as {
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(sym);
    }
    // A ctor is no trait's fn: it runs as itself, with no world asked (build_from's refs loop).
    let is_ctor = program.fn_index(called.item).is_some_and(|c| program.function(c).kind == FnKind::Ctor);
    let selected = match is_ctor {
        true => Ok((called.item, called.args)),
        false => program.resolve(i, called.item, called.args, ground),
    };
    let (item, resolved) = match selected {
        Ok(r) => r,
        Err(error) => {
            let at = crate::callee_shown::self_form(ground.view(), called.args);
            return Err(RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error });
        }
    };
    let callee = Instance { item, args: resolved };
    // `at`: the arguments of the instance whose body makes this call (its `Self` first).
    let Some(c) = program.fn_index(callee.item) else {
        let refusal = callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NotTabled, ground.view());
        if callee.item == called.item {
            // The call's own item: its caller's check tabled every callee it names, so no row
            // is owed by a check of it.
            return Err(refusal);
        }
        return Ok(Named1::Untabled(callee.item, refusal));
    };
    let target = program.function(c);
    if target.kind == FnKind::Foreign {
        return Ok(Named1::C(target.symbol));
    }
    if bodied(program, adts.4.checked, c) {
        return reach.declare(target.key, callee, ground, alloc).map(Named1::Instance).map_err(sym);
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
) -> Result<(), RunError> {
    let function = program.function(i);
    let name = || function.named();
    let fail = |why| RunError::Vtable { function: name(), principal: want.principal, why };
    let full = || fail(VtableFail::Build(BuildError::Malformed { what: "the ground table is full" }));
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
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
        for &(_, to) in table.relocs.iter() {
            let VtableTarget::Fn(f) = to else { continue };
            if let Some(c) = crate::front2::coroutines::owner_of(program, adts, f, ground) {
                reach.declare(program.function(c).key, f, ground, alloc).map_err(sym)?;
                continue;
            }
            let (item, args) = match Some(f.item) == glue_item {
                true => (f.item, f.args),
                false => match program.resolve(i, f.item, f.args, &mut *ground) {
                    Ok(r) => r,
                    Err(error) => {
                        let at = crate::callee_shown::self_form(ground.view(), f.args);
                        return Err(RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), at, error });
                    }
                },
            };
            let Some(c) = program.fn_index(item).filter(|&c| bodied(program, adts.4.checked, c)) else {
                return Err(callee_refusal(program, name(), at, Instance { item, args }, crate::callee_shown::CalleeGap::VtableWord, ground.view()));
            };
            reach.declare(program.function(c).key, Instance { item, args }, ground, alloc).map_err(sym)?;
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

/// THE RECORD WALK'S MAKER, held by the parent for the whole program (D124): one owned session
/// over the names READ FROM RECORDS. [`Maker::new`] resolves, reads and checks nothing; the
/// first [`Make::make`] constructs the lazy program over the records (no `resolve_crates`
/// anywhere under it), and every make after it checks only the bodies its instance needs
/// (`check_one`). A run that finds every record constructs no program and reads no record.
///
/// What the records could not answer is in their log (front2 source.rs): after each make the
/// log is read, and a make over a log that is not empty is refused
/// ([`LazyRefusal::RecordsVoid`]), as is every make after it (the session is void).
pub struct Maker<'n> {
    names: NamesRecords<'n>,
    entry: Option<ItemKey>,
    made: Option<(RecordsProgram<'n>, MakerState)>,
    isa: Isa,
    overflow: OverflowChecks,
    panic: blaze_build::Panic,
    alloc: Alloc<'static>,
    checks: CheckCount,
}

impl<'n> Maker<'n> {
    /// The maker over `names`. `entry`: the checker key of the fn the process starts at, as
    /// `LazyProgram::from_records` takes it (nothing lowers it until a make asks). `builder`:
    /// the records' builder; its ISA, overflow setting and panic strategy are the makes' (rule
    /// 21: one source, nothing to compare). `alloc`: the `'static` heap the lazy program's
    /// tables live in (`Store::new`, `MakerState`). Cheap: reads, resolves and checks nothing.
    pub fn new(names: NamesRecords<'n>, entry: Option<ItemKey>, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n> {
        Maker {
            names,
            entry,
            made: None,
            isa: builder.isa,
            overflow: builder.settings.overflow_checks,
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

    /// The lazy program, once a miss made it.
    pub fn program(&self) -> Option<&RecordsProgram<'n>> {
        self.made.as_ref().map(|(p, _)| p)
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
        if self.made.is_none() {
            let NamesRecords { records, frame, lang_crates, body_sources } = self.names;
            let mut program = RecordsProgram::from_records(records, frame, lang_crates, self.entry, self.alloc).map_err(RunError::Front2)?;
            if let Some(sources) = body_sources {
                program.set_body_sources(sources);
            }
            // The start, by the one rule run/special.rs `roots` takes (the seam's `rust_start`):
            // a refusal here is the parent's own roots refusal, met again, never passed over.
            program.decide_start().map_err(RunError::Roots)?;
            self.checks.constructed += 1;
            // A lazy program holds no checked body when made: 0, counted as the claim reads it.
            self.checks.by_construction += (0..program.fn_count()).filter(|&c| program.function(c).body.is_some()).count();
            let state = MakerState::new(&program, tabled, self.isa, self.overflow, self.panic, self.alloc)?;
            self.made = Some((program, state));
        }
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `Maker::make`)" });
        };
        make_instance(program, state, &mut self.checks, tabled, name, entry)
    }
}

impl Make for Maker<'_> {
    type Error = RunError;

    fn make(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
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

/// `blaze run`'s code records by D106: from `entry` (the program's `main`, by its checker key:
/// the command reads it from crate 0's names record) every instance reached is FOUND in `store`
/// by its key from text (`ids`) or made by `maker` and put. Returns the walk (its rows: symbol,
/// key, tabled fn, name, how) for the launcher, and the counts line.
pub fn walk_records<M: Make<Error = RunError>>(
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
