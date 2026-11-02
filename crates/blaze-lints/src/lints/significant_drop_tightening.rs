//! `significant_drop_tightening`: a lock guard kept past its last use, through a call that
//! could run without the lock.
//!
//! Follows clippy's `clippy_lints/src/significant_drop_tightening.rs` (MIT or Apache-2.0),
//! read from indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the
//! pinned nightly's (rust-lang/rust 12433c30). As in clippy: a guard bound by `let` in a
//! function written in the file, used, and then held while the function goes on to other
//! calls, until its scope ends. Unlike clippy, which walks the statements of the guard's block,
//! this reads the body's runtime MIR:
//!
//! - a use is any mention of the guard or of a value borrowed from it (a reference into it, a
//!   call's result whose type may hold a borrow of an argument that is one), not its drop at
//!   the end of its scope; so `let n = guard.len();` ends the guard's uses, and
//!   `let first = &guard[0];` keeps them going while `first` is used;
//! - the calls held through are the calls written in the source (`from_hir_call`) on a path
//!   where the guard may hold a value and is used again on no path after; across branches and
//!   loops, a guard given a new value in the next round of a loop is a new value;
//! - a guard never used is left alone: holding a lock for a section is what it is for;
//! - only std's and `lock_api`'s lock guards ([`guards`](super::guards)), not every type
//!   marked `#[has_significant_drop]`, which the front end does not read;
//! - closures and `async` blocks in the function are bodies of their own, each linted.
//!
//! No fix is offered: where the guard's scope should end is the author's to say.

use hir_def::DefWithBodyId;
use hir_ty::{
    mir::{
        BasicBlockId, LocalId, MirBody, MirSpan, Operand, OperandKind, Rvalue, StatementKind,
        StoredPlace, TerminatorKind,
    },
    next_solver::{Ty, TyKind},
};
use rustc_type_ir::{TypeVisitableExt as _, inherent::IntoKind as _};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::{
    Implemented,
    guards::{self, Guards, Kind},
};
use crate::check::Cx;

const LINT: Implemented = Implemented::SignificantDropTightening;

/// One guard held past its last use, before it is reported.
struct Held {
    /// Where the guard is given its value.
    at: TextRange,
    /// The first call it is held through.
    call: TextRange,
    message: String,
    guard: String,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(function) = ast::Fn::cast(node) else { return };
    if function.body().is_none() || !cx.is_written(node) {
        return;
    }
    let mut held = find(cx, function);
    held.sort_by_key(|it| (it.at.start(), it.at.end()));
    for it in held {
        let span = cx.span(it.at);
        let call = cx.span(it.call);
        let diagnostic = cx.lint(LINT, node, span, it.message);
        diagnostic.span_note(call, format!("this call runs with {} still held", Kind::Lock.held()));
        diagnostic.help(format!(
            "drop `{0}` after its last use (`drop({0});`), or end its scope with a block, so other threads \
             can take the lock sooner",
            it.guard
        ));
    }
}

fn find<'t>(cx: &Cx<'_, 't>, function: ast::Fn<'t>) -> Vec<Held> {
    // alloc: the guards found in one function, reported at the edge
    let mut out = Vec::new();
    let Some(f) = cx.fn_def(function) else { return out };
    let Some(analysis) = cx.analysis() else { return out };
    // The walk of a body meets the functions nested in it, each linted in its own body's query.
    if analysis.def() != hir::DefWithBody::from(f) {
        return out;
    }
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return out };
    let db = cx.db();
    let guards = Guards::new(cx);
    let function_name = guards::function_name(cx, analysis);
    for body in guards::bodies(db, analysis, def) {
        let mir = body.mir;
        let owner = match body.expr {
            None => function_name.clone(), // clone: once per body
            Some(_) if body.coroutine => format!("an `async` block in {function_name}"),
            Some(_) => format!("a closure in {function_name}"),
        };
        for (binding, &local) in mir.binding_locals.iter() {
            let name = analysis.body()[binding].name.as_str();
            if name.starts_with('_') {
                continue;
            }
            let ty = analysis.ty(mir.locals[local].ty.as_ref());
            if ty.as_adt().and_then(|adt| guards.kind(db, adt)) != Some(Kind::Lock) {
                continue;
            }
            let Some(call) = held_through(mir, local) else { continue };
            let Some(at) = guards::first_assigned(mir, local).and_then(|it| guards::range_of(analysis, it)) else {
                continue;
            };
            let Some(call) = guards::range_of(analysis, call) else { continue };
            let message = format!(
                "{} holds {} in {owner} after its last use",
                guards::say_local(cx, analysis, mir, local),
                Kind::Lock.held()
            );
            out.push(Held { at, call, message, guard: name.to_owned() }); // alloc: reported at the edge
        }
    }
    out
}

/// The first call, by its span, that `guard` is held through: one written in the source,
/// with the guard maybe holding a value before it and used on no path after it.
fn held_through(mir: &MirBody<'_>, guard: LocalId) -> Option<MirSpan> {
    let derived = derived(mir, guard);
    let uses = |places: &mut dyn FnMut(&mut dyn FnMut(&StoredPlace))| {
        let mut any = false;
        places(&mut |place| any |= derived[index(place.local)]);
        any
    };
    let blocks = mir.basic_blocks.len();
    let mut used_somewhere = false;
    // Backward: whether a use comes on some path from the start of each block, the guard's
    // new value ending the old one's.
    // alloc: a flag per block of one body
    let mut live_in = vec![false; blocks];
    // Every pass that changes nothing ends it; each change sets a flag that stays set.
    for _ in 0..=blocks {
        let mut changed = false;
        for (id, block) in mir.basic_blocks.iter().rev() {
            if block.is_cleanup {
                continue;
            }
            let Some(terminator) = &block.terminator else { continue };
            let mut live = terminator.kind.successors().any(|it| live_in[block_index(it)]);
            if let TerminatorKind::Call { destination, .. } = &terminator.kind
                && whole(destination, guard)
            {
                live = false;
            }
            if uses(&mut |f| terminator_places(&terminator.kind, guard, f)) {
                live = true;
                used_somewhere = true;
            }
            for statement in block.statements.iter().rev() {
                if let StatementKind::Assign(place, _) = &statement.kind
                    && whole(place, guard)
                {
                    live = false;
                }
                if uses(&mut |f| statement_places(&statement.kind, f)) {
                    live = true;
                    used_somewhere = true;
                }
            }
            if live && !live_in[block_index(id)] {
                live_in[block_index(id)] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !used_somewhere {
        return None;
    }
    let init_in = guards::maybe_init(mir, guard);
    let mut found = None;
    for (id, block) in mir.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { target: Some(target), from_hir_call: true, .. } = &terminator.kind else {
            continue;
        };
        if block.is_cleanup
            || live_in[block_index(*target)]
            || matches!(terminator.span, MirSpan::Unknown)
            || uses(&mut |f| terminator_places(&terminator.kind, guard, f))
            || !guards::init_before_terminator(mir, id, guard, init_in[block_index(id)])
        {
            continue;
        }
        found = Some(terminator.span);
        break;
    }
    found
}

/// The locals that hold `guard` or a borrow of it: references into it, and values made from
/// one whose type may hold a borrow, the guard itself first.
fn derived(mir: &MirBody<'_>, guard: LocalId) -> Vec<bool> {
    // alloc: a flag per local of one body
    let mut set = vec![false; mir.locals.len()];
    set[index(guard)] = true;
    let may_borrow = |local: LocalId| borrows(mir.locals[local].ty.as_ref());
    let from = |set: &[bool], it: &Operand| match &it.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => set[index(place.local)],
        _ => false,
    };
    // Every pass that changes nothing ends it; each change adds a local that stays.
    for _ in 0..=mir.locals.len() {
        let mut changed = false;
        for (_, block) in mir.basic_blocks.iter() {
            for statement in &block.statements {
                let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                if set[index(place.local)] {
                    continue;
                }
                let made = match rvalue {
                    Rvalue::Ref(_, it) => set[index(it.local)],
                    Rvalue::CopyForDeref(it) => set[index(it.local)] && may_borrow(place.local),
                    Rvalue::Len(_) | Rvalue::Discriminant(_) => false,
                    _ => {
                        let mut any = false;
                        rvalue.for_each_operand(&mut |it| any |= from(set.as_slice(), it));
                        any && may_borrow(place.local)
                    }
                };
                if made {
                    set[index(place.local)] = true;
                    changed = true;
                }
            }
            if let Some(terminator) = &block.terminator
                && let TerminatorKind::Call { args, destination, .. } = &terminator.kind
                && !set[index(destination.local)]
                && args.iter().any(|it| from(set.as_slice(), it))
                && may_borrow(destination.local)
            {
                set[index(destination.local)] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    set
}

/// Whether a value of `ty` may hold a borrow: a reference, a raw pointer, or a type with a
/// lifetime in it.
fn borrows(ty: Ty<'_>) -> bool {
    ty.has_regions() || matches!(ty.kind(), TyKind::RawPtr(..))
}

/// Calls `f` with each place a statement reads, borrows or writes into, but not the whole
/// local an assignment gives a value to.
fn statement_places(kind: &StatementKind, f: &mut dyn FnMut(&StoredPlace)) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            if !place.projection.is_empty() {
                f(place);
            }
            match rvalue {
                Rvalue::Ref(_, it) | Rvalue::Len(it) | Rvalue::Discriminant(it) | Rvalue::CopyForDeref(it) => f(it),
                _ => rvalue.for_each_operand(&mut |it| operand_place(it, &mut *f)),
            }
        }
        StatementKind::InlineAsm(_) => kind.for_each_operand(&mut |it| operand_place(it, &mut *f)),
        _ => {}
    }
}

/// Calls `f` with each place a terminator reads, borrows or writes into, but not the whole
/// local a call returns into, nor `guard` dropped at the end of its scope.
fn terminator_places(kind: &TerminatorKind, guard: LocalId, f: &mut dyn FnMut(&StoredPlace)) {
    match kind {
        TerminatorKind::Drop { place, .. } if whole(place, guard) => {}
        TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => f(place),
        TerminatorKind::Call { destination, .. } if !destination.projection.is_empty() => f(destination),
        _ => {}
    }
    kind.for_each_operand(&mut |it| operand_place(it, &mut *f));
}

fn operand_place(it: &Operand, f: &mut dyn FnMut(&StoredPlace)) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &it.kind {
        f(place);
    }
}

fn whole(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local && place.projection.is_empty()
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

#[cfg(test)]
mod tests {
    use super::super::drops_tests::lint_only;

    const LINT: &[&str] = &["significant_drop_tightening"];
    /// The end of a finding's message line; the help says "after its last use" too.
    const FOUND: &str = "after its last use\n";

    #[test]
    fn a_guard_held_through_a_call_after_its_last_use_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn count(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    work();
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`guard`, a `MutexGuard<"), "{found}");
        assert!(found.contains("holds the lock in `count` after its last use"), "{found}");
        assert!(found.contains("this call runs with the lock still held"), "{found}");
        assert!(found.contains("`drop(guard);`"), "{found}");
    }

    /// Used after the call, dropped before it, never used (a critical section), named `_`,
    /// or a borrow of it still used after the call: none is held past its last use. A
    /// `RefCell` borrow is no lock.
    #[test]
    fn a_guard_used_later_dropped_unused_or_still_borrowed_is_left_alone() {
        let found = lint_only(
            LINT,
            r#"
use std::{cell::RefCell, sync::Mutex};
pub fn work() {}
pub fn used_after(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    work();
    n + guard.len()
}
pub fn dropped(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    std::mem::drop(guard);
    work();
    n
}
pub fn section(m: &Mutex<Vec<u32>>) {
    let guard = m.lock().unwrap();
    work();
}
pub fn underscore(m: &Mutex<Vec<u32>>) -> usize {
    let _guard = m.lock().unwrap();
    let n = _guard.len();
    work();
    n
}
pub fn borrowed(m: &Mutex<Vec<u32>>) -> u32 {
    let guard = m.lock().unwrap();
    let first = guard.first();
    work();
    match first { Some(x) => *x, None => 0 }
}
pub fn cell(c: &RefCell<Vec<u32>>) -> usize {
    let b = c.borrow();
    let n = b.len();
    work();
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    /// The guard of the next round of a loop is a new value: its uses are not the old one's.
    #[test]
    fn a_guard_made_again_in_each_round_of_a_loop_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn rounds(m: &Mutex<Vec<u32>>, k: usize) {
    let mut i = 0;
    while i < k {
        let guard = m.lock().unwrap();
        let n = guard.len();
        work();
        i += n;
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("in `rounds` after its last use"), "{found}");
    }

    #[test]
    fn a_renamed_lock_apis_guard_and_one_written_in_a_macro_call_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod locks { pub use parking_lot::Mutex as Lock; }
use locks::Lock;
use std::sync::Mutex;
pub fn work() {}
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub fn parked(m: &Lock<Vec<u32>>) -> usize {
    let held = m.lock();
    let n = held.len();
    work();
    n
}
item! {
    pub fn handed(m: &Mutex<Vec<u32>>) -> usize {
        let guard = m.lock().unwrap();
        let n = guard.len();
        work();
        n
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`held`, a `MutexGuard<"), "{found}");
        assert!(found.contains("in `parked` after its last use"), "{found}");
        assert!(found.contains("in `handed` after its last use"), "{found}");
    }
}
