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
//! - `build_coroutines` (an owner fn's checked body read from the whole table): [`coroutine_demand`],
//!   the same stack with each owner checked on demand first. build_from keeps its own until the
//!   switch to the walk deletes it (the deletion wiring names it).
//! - the reference loop and the vtable loop inside build_from's step (slots, keys): [`name_refs`]
//!   and [`vtable_rows`], by instance NAME, no slot, no instance key (the walk tells rows apart by
//!   symbol).
//!
//! THE ONE GROUND TABLE lives across instances in [`MakerState`]: coroutine states, pulled
//! constants and the coroutine stash are keyed by interned types of it, as in build_from.
//!
//! Allocators: `Alloc<'static>` handed in by the caller (the tables this calls into take
//! `Alloc<'static>`: `CoroutineTable::of`, `Wanted::new`). `Global` is named only where run.rs's
//! existing types fix it (`Bodies::const_at`, `BuiltParts::args`, blaze-build's `Built<Global>`).

use core::cell::OnceCell;

use super::*;

use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_own::AVec;

use crate::body_ids::{BodyIds, Counts, EntryForm, Make, Walk, WalkError, each_item};
use crate::code_records::{Builder, Callee, CodeRecordError, CodeStore, Declared, InstanceNames, make_entry};
use crate::front2::const_at::{ConstNeed, ConstTable, Needs};
use crate::front2::coroutines::{CoroutineTable, Coroutines, Wanted};
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
    langs: BackLangs,
    ground: PortableTys<'static>,
    layouts: Layouts<Alloc<'static>>,
    /// Each coroutine instance built, its three functions waiting for their instances.
    stash: Vec<BuiltParts, Global>,
    needs: Needs<Global>,
    /// Whether a made instance touched a thread-local: the program then needs the getter's
    /// and ender's records (code_records.rs `RecordRun::getter`/`ender`).
    thread_locals: bool,
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
    /// A coroutine instance must be built (its state, or its three functions) first.
    Coroutine(Wanted),
}

fn tables_gone(key: ItemKey) -> RunError {
    RunError::OneBody { key, why: "the maker's tables were left taken by an earlier failed step (make_one.rs `MakerState::tables`)" }
}

impl MakerState {
    /// The state for `program`, before its first instance. Refuses a front end that evaluates
    /// constants for the whole program only (`evaluates_at_args` false), naming `first`.
    pub fn new<P: FrontEnd>(program: &P, first: ItemKey, isa: Isa, overflow: OverflowChecks, alloc: Alloc<'static>) -> Result<MakerState, RunError> {
        if !program.evaluates_at_args() {
            return Err(RunError::OneBody {
                key: first,
                why: "this front end evaluates its constants for the whole program (run.rs `eval_consts`); the record walk takes one that evaluates each at its arguments (front2)",
            });
        }
        let langs = BackLangs::of(program)?;
        let empty: &[Option<Checked<'static>>] = &[];
        let tables = Tables {
            closures: ClosureTable::of(program, empty),
            coroutines: Coroutines::of(program, empty, alloc),
            const_at: ConstTable::new_in(Global),
        };
        let work = Work {
            alloc,
            isa,
            overflow,
            langs,
            ground: PortableTys::new_in(alloc),
            layouts: Layouts::new_in(alloc),
            stash: Vec::new_in(Global),
            needs: Needs::new_in(Global),
            thread_locals: false,
        };
        Ok(MakerState { asked: blaze_own::Vec::new_in(alloc), checked: blaze_own::Vec::new_in(alloc), tables: Some(tables), work })
    }

    /// Fn `key` checked, once a run: its index, `None` when the front end has no body for it.
    /// A newly checked body with a closure or an async block re-makes the closure and coroutine
    /// tables over the bodies checked so far.
    fn ensure<P: FrontEnd>(&mut self, program: &mut P, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
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
                    tables.closures = ClosureTable::of(&*program, &self.checked);
                    tables.coroutines.table = CoroutineTable::of(&*program, &self.checked, self.work.alloc);
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
    // The build, with a coroutine demand (a future held by value whose state is not made yet)
    // met by building that coroutine first: a demand, not a second try; bounded (rule 18).
    let mut demands = 0usize;
    loop {
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow };
        let step = build_one(&*program, &bodies, work, i, instance, entry);
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        match step? {
            Step::Entry => break,
            Step::Coroutine(wanted) => {
                demands += 1;
                if demands > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "a body whose build asked for more than 4096 coroutine states" };
                    return Err(RunError::Build { function: program.function(i).named(), error, range: None });
                }
                coroutine_demand(program, state, checks, wanted)?;
            }
        }
    }
    program.done(i);
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
        return Err(callee_refusal(&*program, function.named(), instance.item));
    };
    let malformed = |_| RunError::Build { function: function.named(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    args.extend_from_slice(work.ground.view().list(instance.args).map_err(malformed)?);
    let t = tables.take().ok_or_else(|| tables_gone(function.key))?;
    let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow };
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
        let item = match refusal.need {
            ConstNeed::Item(k) => k,
            ConstNeed::Block { owner, .. } => owner,
        };
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
        return Err(callee_refusal(program, name(), instance.item));
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
            _ => return Err(callee_refusal(program, name(), instance.item)),
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
    // One of a coroutine's three functions: its instance is built once for all three.
    let from_coroutine = match coroutine {
        None => None,
        Some((of, part)) => {
            let at = work.stash.iter().position(|b| b.owner == of.owner && b.ordinal == of.ordinal && b.args[..] == args[..]);
            let Some(at) = at else {
                return Ok(Step::Coroutine(Wanted::new(of.owner, of.ordinal, &args, work.alloc)));
            };
            let Some(taken) = work.stash[at].parts[part.index()].take() else {
                let error = BuildError::Malformed { what: "a coroutine's function its instance's build did not leave (asked for twice)" };
                return Err(RunError::Build { function: name(), error, range: None });
            };
            Some(taken)
        }
    };
    let view = body.view(types);
    let built = {
        let mut subst = Subst { args: &args, ground: &mut work.ground, scratch: work.alloc };
        if let Some(taken) = from_coroutine {
            Ok(taken)
        } else if let Some(s) = shim {
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
        }
    };
    let built = match built {
        Ok(b) => b,
        Err(BuildError::MissingRecord { item }) if let Some(wanted) = bodies.coroutines.states.take_wanted().filter(|w| w.owner == item) => {
            return Ok(Step::Coroutine(wanted));
        }
        Err(error) => return Err(build_refusal(program, name(), body, error)),
    };
    // Every reference named; each instance it reaches declared by (tabled fn, name).
    work.thread_locals |= !built.thread_locals.is_empty();
    let mut reach = Reach::new_in(work.alloc);
    let mut funcs: AVec<'_, Callee<'_>> = blaze_own::Vec::with_capacity_in(built.refs.len(), work.alloc);
    for &called in built.refs.iter() {
        match name_ref(program, &adts, &mut work.ground, work.alloc, &mut reach, i, called)? {
            Named1::Instance(symbol) => funcs.push(Callee::Symbol(symbol)),
            Named1::C(c_name) => funcs.push(Callee::C(c_name)),
        }
    }
    for &want in built.vtables.iter() {
        vtable_rows(program, &adts, &mut work.ground, &mut work.layouts, work.alloc, &mut reach, i, want)?;
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
    let names = InstanceNames { funcs: &funcs, statics: &statics, vtables: &vtables, tls: &tls_names };
    let mut declared: AVec<'_, Declared<'_>> = blaze_own::Vec::with_capacity_in(reach.rows.len(), work.alloc);
    for n in 0..reach.rows.len() {
        let (tabled, name_bytes) = reach.row(n);
        declared.push(Declared { tabled, name: name_bytes });
    }
    let made = make_entry(&built.func, &names, &mut declared, work.isa, work.alloc).map_err(|error| RunError::CodeRecord { function: name(), error })?;
    entry.extend_from_slice(&made);
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
}

/// build_from's reference loop for one reference, by name: a closure's, a coroutine's or a
/// shim's instance tabled as its owner; a trait's fn as the selected impl's; an outside fn by
/// its C name. A reference with no instance is this instance's refusal (no stub on this path).
fn name_ref<'p, P: FrontEnd>(
    program: &'p P,
    adts: &FrontAdts<'_, P>,
    ground: &mut PortableTys<'static>,
    alloc: Alloc<'static>,
    reach: &mut Reach<'_>,
    i: usize,
    called: Instance,
) -> Result<Named1<'p>, RunError> {
    let function = program.function(i);
    let name = || function.named();
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    let tabled_as = crate::front2::coroutines::owner_of(program, adts, called, ground)
        .or_else(|| closure_owner(program, adts, called, ground))
        .or_else(|| crate::fn_shim::fn_shim_owner(program, adts, called, ground, i));
    if let Some(c) = tabled_as {
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(sym);
    }
    let (item, resolved) = program
        .resolve(i, called.item, called.args, ground)
        .map_err(|error| RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), error })?;
    let callee = Instance { item, args: resolved };
    let Some(c) = program.fn_index(callee.item) else {
        return Err(callee_refusal(program, name(), callee.item));
    };
    let target = program.function(c);
    if target.body.is_some() {
        return reach.declare(target.key, callee, ground, alloc).map(Named1::Instance).map_err(sym);
    }
    if target.kind == FnKind::Foreign {
        return Ok(Named1::C(target.symbol));
    }
    Err(callee_refusal(program, name(), callee.item))
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
                false => program
                    .resolve(i, f.item, f.args, &mut *ground)
                    .map_err(|error| RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), error })?,
            };
            let Some(c) = program.fn_index(item).filter(|&c| program.function(c).body.is_some()) else {
                return Err(callee_refusal(program, name(), item));
            };
            reach.declare(program.function(c).key, Instance { item, args }, ground, alloc).map_err(sym)?;
        }
    }
    Ok(())
}

/// `build_coroutines` with each owner's body checked on demand: the coroutine instance `first`
/// and, before it, each coroutine whose state its build asks for, by a stack.
fn coroutine_demand<P: FrontEnd>(program: &mut P, state: &mut MakerState, checks: &mut CheckCount, first: Wanted) -> Result<(), RunError> {
    let mut stack: AVec<'_, Wanted> = blaze_own::Vec::new_in(state.work.alloc);
    stack.push(first);
    while let Some(top) = stack.last() {
        let (owner, ordinal) = (top.owner, top.ordinal);
        let made = state.tables.as_ref().ok_or_else(|| tables_gone(owner))?.coroutines.states.made(owner, ordinal, &top.args);
        if made {
            stack.pop();
            continue;
        }
        let Some(c) = state.ensure(program, owner, checks)? else {
            let function = program.function(program.entry().unwrap_or(0)).named();
            return Err(RunError::Build { function, error: BuildError::UnsupportedSig { what: "an async block whose owner fn has no checked body" }, range: None });
        };
        let MakerState { checked, tables, work, .. } = &mut *state;
        let Some(top) = stack.last() else { break };
        let t = tables.take().ok_or_else(|| tables_gone(owner))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow };
        let built = {
            let function = program.function(c);
            match (function.body, bodies.checked.get(c).and_then(Option::as_ref)) {
                (Some((body, _)), Some(done)) => {
                    let view = body.view(program.types());
                    let adts = FrontAdts(&*program, c, &[], &work.langs, &bodies);
                    let mut subst = Subst { args: &top.args, ground: &mut work.ground, scratch: work.alloc };
                    Ok((blaze_build::build_coroutine(&view, done, ordinal, &mut subst, &adts, Global), body))
                }
                _ => Err(RunError::Build { function: function.named(), error: BuildError::UnsupportedSig { what: "an async block whose owner fn has no checked body" }, range: None }),
            }
        };
        let next = match built {
            Err(e) => Err(e),
            Ok((Ok(built), _)) => {
                bodies.coroutines.states.record(owner, ordinal, &top.args, built.state);
                let mut args = Vec::with_capacity_in(top.args.len(), Global);
                args.extend_from_slice(&top.args);
                work.stash.push(BuiltParts { owner, ordinal, args, parts: [Some(built.poll), Some(built.resume), Some(built.drop)] });
                Ok(None)
            }
            Ok((Err(BuildError::MissingRecord { item }), _)) => match bodies.coroutines.states.take_wanted().filter(|w| w.owner == item) {
                Some(next) => Ok(Some(next)),
                None => Err(RunError::ItemRefused { function: program.function(c).named(), error: BuildError::MissingRecord { item }, item, path: program.item_named(item) }),
            },
            Ok((Err(error), body)) => Err(build_refusal(&*program, program.function(c).named(), body, error)),
        };
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        match next? {
            None => {
                stack.pop();
            }
            Some(next) => {
                let refuse = |what| RunError::Build { function: program.function(c).named(), error: BuildError::UnsupportedSig { what }, range: None };
                if stack.iter().any(|s| s.is(next.owner, next.ordinal, &next.args)) {
                    return Err(refuse("an async block whose state holds itself by value (E0733: recursion in an async fn requires boxing)"));
                }
                if stack.len() >= MOST_HELD_COROUTINES {
                    return Err(refuse("more than 4096 futures held one inside another by value"));
                }
                stack.push(next);
            }
        }
    }
    Ok(())
}

/// The record walk's maker over a program constructed ON THE FIRST MISS only: a run that finds
/// every record constructs no program and resolves no names. One type for both instantiations
/// of D106 (today's `Front2Program`, piece Y's lazy program): only `P` and `construct` differ.
pub struct OnFirstMiss<P, C> {
    construct: C,
    made: Option<(P, MakerState)>,
    isa: Isa,
    overflow: OverflowChecks,
    alloc: Alloc<'static>,
    checks: CheckCount,
}

impl<P: FrontEnd, C: FnMut() -> Result<P, RunError>> OnFirstMiss<P, C> {
    pub fn new(construct: C, isa: Isa, overflow: OverflowChecks, alloc: Alloc<'static>) -> OnFirstMiss<P, C> {
        OnFirstMiss { construct, made: None, isa, overflow, alloc, checks: CheckCount::default() }
    }

    /// Whether an instance made in this run touched a thread-local. A FOUND record's use of
    /// one is said by bzcode03's declared rows (kinds 4 and 5); until that form lands the
    /// caller makes the getter's and ender's records when this is true or its own records say so.
    pub fn made_thread_locals(&self) -> bool {
        self.made.as_ref().is_some_and(|(_, s)| s.work.thread_locals)
    }

    /// The checks the makes cost so far.
    pub fn checks(&self) -> CheckCount {
        self.checks
    }

    /// The program, once a miss made it.
    pub fn program(&self) -> Option<&P> {
        self.made.as_ref().map(|(p, _)| p)
    }
}

impl<P: FrontEnd, C: FnMut() -> Result<P, RunError>> Make for OnFirstMiss<P, C> {
    type Error = RunError;

    fn make(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if self.made.is_none() {
            let program = (self.construct)()?;
            self.checks.constructed += 1;
            self.checks.by_construction += (0..program.fn_count()).filter(|&c| program.function(c).body.is_some()).count();
            let state = MakerState::new(&program, tabled, self.isa, self.overflow, self.alloc)?;
            self.made = Some((program, state));
        }
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `OnFirstMiss::make`)" });
        };
        make_instance(program, state, &mut self.checks, tabled, name, entry)
    }
}

/// The constructor of today's `Front2Program`, made only when the first miss calls it (the
/// order `build_records_new` makes its inputs in), handed to `then`: a run that finds every
/// record reads no toolchain and resolves no names. Each input is a local here, declared before
/// what borrows it, so nothing borrows itself; `construct` makes the program once and refuses a
/// second call by name.
pub fn with_front2<R>(
    files: &[SourceFile<'_>],
    root: &str,
    new: NewFront<'_>,
    alloc: Alloc<'static>,
    then: impl for<'c> FnOnce(&mut dyn FnMut() -> Result<Front2Program<'c>, RunError>) -> R,
) -> R {
    let tc_cell: OnceCell<Toolchain> = OnceCell::new();
    let texts_cell: OnceCell<ProgramTexts> = OnceCell::new();
    let user_cell = OnceCell::new();
    let inputs_cell = OnceCell::new();
    let crates_cell = OnceCell::new();
    let names_cell = OnceCell::new();
    let frame_cell: OnceCell<blaze_mem::Frame> = OnceCell::new();
    let (tc_cell, texts_cell, user_cell, inputs_cell, crates_cell, names_cell, frame_cell) =
        (&tc_cell, &texts_cell, &user_cell, &inputs_cell, &crates_cell, &names_cell, &frame_cell);
    let mut construct = move || -> Result<Front2Program<'_>, RunError> {
        if tc_cell.get().is_some() {
            return Err(RunError::OneBody { key: ItemKey(0), why: "the program was asked for twice in one run (make_one.rs `with_front2`)" });
        }
        let tc = crate::library_set::toolchain_for(files, root, new, alloc)?;
        let tc = tc_cell.get_or_init(|| tc);
        let texts = ProgramTexts::new(files, root, new, tc)?;
        let texts = texts_cell.get_or_init(|| texts);
        let user = user_cell.get_or_init(|| texts.user());
        let inputs = Inputs::new(user, tc).map_err(RunError::Crates)?;
        let inputs = inputs_cell.get_or_init(|| inputs);
        let crates = crates_cell.get_or_init(|| inputs.crates());
        let names = inputs.resolve(crates).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
        let names = names_cell.get_or_init(|| names);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(|e| RunError::Names { shown: std::format!("the command's frame: {e}").into_boxed_str() })?;
        let frame = frame_cell.get_or_init(|| frame);
        Front2Program::new(names, frame, files).map_err(RunError::Front2)
    };
    then(&mut construct)
}

/// What one record walk of `blaze run` did.
pub struct RecordsWalked {
    /// The walk's rows (symbol, key, tabled fn, name, how), for the launcher.
    pub walk: Walk<'static>,
    pub counts: Counts,
    pub checks: CheckCount,
    /// An instance made in this run touched a thread-local (`OnFirstMiss::made_thread_locals`).
    pub made_thread_locals: bool,
}

/// `blaze run`'s records by D106 over today's front2, the program made only on the first miss:
/// the run path when the command hands over its [`BodyIds`] (with none, the command keeps
/// calling `build_records_new`, unchanged). `entry` is `main`'s checker key, from crate 0's
/// names record (the command's; `exported_entry` over the records names source when piece Y
/// lands).
#[allow(clippy::too_many_arguments)]
pub fn run_records(
    files: &[SourceFile<'_>],
    root: &str,
    new: NewFront<'_>,
    entry: ItemKey,
    ids: &dyn BodyIds,
    store: &mut dyn CodeStore,
    builder: &Builder,
    producer: &ProducerId<CodeBuilder>,
    alloc: Alloc<'static>,
) -> Result<RecordsWalked, RecordWalkError> {
    let key = builder.settings.overflow_checks;
    if new.overflow != key {
        return Err(RecordWalkError::Setup(RunError::OverflowDisagrees { walk: new.overflow, key }));
    }
    with_front2(files, root, new, alloc, |construct| {
        let mut maker = OnFirstMiss::new(construct, builder.isa, key, alloc);
        let (walk, counts) = walk_records(entry, ids, store, builder, producer, &mut maker, alloc)?;
        Ok(RecordsWalked { walk, counts, checks: maker.checks(), made_thread_locals: maker.made_thread_locals() })
    })
}

/// Why the record walk over `blaze run`'s records stopped.
#[derive(Debug)]
pub enum RecordWalkError {
    /// The walk's inputs disagree (its overflow setting and its records' key).
    Setup(RunError),
    /// The entry's name could not be made.
    Entry(crate::symbols::SymbolError),
    /// The walk, the store or one make refused.
    Walk(WalkError<RunError>),
}

impl core::fmt::Display for RecordWalkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordWalkError::Setup(e) => e.fmt(f),
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
