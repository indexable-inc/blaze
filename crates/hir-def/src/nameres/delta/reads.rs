//! What a maintainer run reads: the read set the read index keeps (design §6). Every read of a
//! module goes through `DefMap::module` and `DefMap::name_in_module` of a
//! `ModulesSlot::Maintained` view, or a published scope (`ScopeSlot::Input`, another crate's or
//! one of this crate the run does not hold), and each notes it here. A name read is
//! `(module, Some(name))`, negative or not; anything else (a whole scope, the children, the
//! textual macros, the row) is `(module, None)`.
//!
//! A read is noted with its *reader*: the module whose import the unit is resolving, or whose
//! macro directive it is resolving or collecting the expansion of ([`reading`]). A read made
//! outside both has no reader, and the run counts it as read by every group it holds.
//!
//! The notes go to the calling thread's list, which is on only while [`capture`] runs: a run is
//! on one thread, and the queries it asks that run elsewhere read no maintained view.

use std::cell::{Cell, RefCell};

use hir_expand::name::Name;

use crate::ModuleId;

/// `(reader, (module, name))`.
pub(crate) type Read = (Option<ModuleId>, (ModuleId, Option<Name>));

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static READER: Cell<Option<ModuleId>> = const { Cell::new(None) };
    static READS: RefCell<Vec<Read>> = const { RefCell::new(Vec::new()) }; // alloc: empty, grown once per thread and kept
}

// Each access to `ON`, `READER` or `READS` is in a function of its own, never inlined: a run
// that waits between two may resume on another thread, and an inlined access may reuse the
// address of the first thread's slot (docs/v2/plan/lanes/may-suspend.md).

/// The name a glob of an enum, or a path through it to a variant (`use E::V`), is noted reading
/// in the enum's module, `(module, Some(it))`: no binding has it, so a glob of the module
/// (`(module, None)`) is not woken with the enum's readers, and only a write touching an enum
/// there wakes them (`delta::enum_glob_wakes`).
pub(crate) fn enum_glob() -> Name {
    Name::new_symbol_root(intern::Symbol::intern("{enum glob}"))
}

/// The name an import of a trait's items (`use Trait::item`, `use Trait::*`) is noted reading
/// in the trait's module, as [`enum_glob`] for an enum's variants: an item is no binding of the
/// module, and a write touching the trait wakes them (`delta::enum_glob_wakes`).
pub(crate) fn trait_import() -> Name {
    Name::new_symbol_root(intern::Symbol::intern("{trait import}"))
}

/// Notes a read of `module` (of `name` in it, or of the module whole).
#[inline(never)]
pub(crate) fn note(module: ModuleId, name: Option<Name>) {
    if ON.get() {
        let reader = READER.get();
        // alloc: the thread's list keeps its capacity from run to run.
        READS.with_borrow_mut(|it| it.push((reader, (module, name))));
    }
}

/// Whether a run is capturing on this thread: a published scope notes its reads only then.
#[inline(never)]
pub(crate) fn on() -> bool {
    ON.get()
}

/// The reader reads are noted with until the guard drops, when the one before is back.
#[must_use]
pub(crate) struct Reading {
    before: Option<ModuleId>,
}

impl Drop for Reading {
    fn drop(&mut self) {
        replace_reader(self.before);
    }
}

#[inline(never)]
fn replace_reader(reader: Option<ModuleId>) -> Option<ModuleId> {
    READER.replace(reader)
}

/// Reads from now until the guard drops are `module`'s.
#[inline]
pub(crate) fn reading(module: ModuleId) -> Reading {
    carry_capture();
    Reading { before: replace_reader(Some(module)) }
}

/// Carries [`ON`], [`READER`] and [`READS`] with each job on a fiber across its waits
/// ([`blaze_query::carry`]): a run capturing its reads that waits on a query goes on capturing
/// into its own list, wherever it resumes, and another job on the thread meanwhile captures
/// nothing.
fn carry_capture() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried_cell(&ON, |on| !on),
            put: |it| stdx::lend::put_carried_cell(&ON, |on| !on, it),
        });
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried_cell(&READER, Option::is_none),
            put: |it| stdx::lend::put_carried_cell(&READER, Option::is_none, it),
        });
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried(&READS, Vec::is_empty),
            put: |it| stdx::lend::put_carried(&READS, Vec::is_empty, it),
        });
    });
}

/// Runs `f`, then moves what it read to the end of `into` (with repeats; the caller sorts).
/// A panic in `f` leaves capturing off and the list empty. A run that starts on a thread where
/// another captures (one crate's job that a wait of another crate's pumped onto its stack)
/// captures into a list of its own, and the other's capture is back when it ends.
pub(super) fn capture<R>(into: &mut Vec<Read>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Outer>);
    impl Drop for Restore {
        fn drop(&mut self) {
            restore_capture(self.0.take());
        }
    }
    carry_capture();
    let restore = Restore(start_capture());
    let out = f();
    end_capture(into);
    drop(restore);
    out
}

/// The capture a nested run put aside.
struct Outer {
    reader: Option<ModuleId>,
    reads: Vec<Read>,
}

#[inline(never)]
fn start_capture() -> Option<Outer> {
    let outer = ON.get().then(|| Outer {
        reader: READER.get(),
        // alloc: none, the outer run's list moved aside with its capacity
        reads: READS.with_borrow_mut(std::mem::take),
    });
    READS.with_borrow_mut(|it| it.clear());
    READER.set(None);
    ON.set(true);
    outer
}

#[inline(never)]
fn end_capture(into: &mut Vec<Read>) {
    ON.set(false);
    READS.with_borrow_mut(|it| into.append(it));
}

#[inline(never)]
fn restore_capture(outer: Option<Outer>) {
    READS.with_borrow_mut(|it| it.clear());
    match outer {
        Some(Outer { reader, reads }) => {
            READS.set(reads);
            READER.set(reader);
            ON.set(true);
        }
        None => {
            ON.set(false);
            READER.set(None);
        }
    }
}
