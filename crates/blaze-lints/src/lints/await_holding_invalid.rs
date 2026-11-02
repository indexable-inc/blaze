//! `await_holding_lock` and `await_holding_refcell_ref`: a lock guard, or a `RefCell` borrow,
//! held across an `.await`.
//!
//! Follows clippy's `clippy_lints/src/await_holding_invalid.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: what an `async` body keeps in its future
//! across a suspension, rustc's coroutine witnesses, each holding a std or `lock_api` lock
//! guard (`await_holding_lock`) or a `RefCell`'s `Ref`/`RefMut` (`await_holding_refcell_ref`),
//! linted where it is given its value with a note at each `.await` it is held across. Unlike
//! clippy, which reads the witnesses rustc's MIR building records:
//!
//! - what is held is the front end's own answer, the locals its coroutine layout keeps
//!   ([`MirBody::held_locals_in`], MIR liveness and borrows over the runtime body), so a guard
//!   dropped or moved away before the `.await` is not held, and one used after it is;
//! - a value holds a guard when it is one or one is among its arguments or fields
//!   ([`guards`](super::guards)): `Option<MutexGuard<..>>`, a tuple or a struct with one.
//!
//! [`MirBody::held_locals_in`]: hir_ty::mir::MirBody::held_locals_in

use hir::HirDisplay as _;
use hir_def::DefWithBodyId;
use hir_ty::mir::{BasicBlockId, MirSpan, TerminatorKind};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::{
    Implemented,
    guards::{self, Guards, Kind},
};
use crate::check::Cx;

/// How many of the `.await`s a value is held across get a note.
const NOTED: usize = 3;

/// One value held across `.await`s, before it is reported.
struct Held {
    lint: Implemented,
    at: TextRange,
    awaits: Vec<TextRange>,
    message: String,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(Implemented::AwaitHoldingLock) && !cx.on(Implemented::AwaitHoldingRefcellRef) {
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
        let awaits: Vec<_> = it.awaits.iter().map(|&at| cx.span(at)).collect(); // alloc: a few spans
        let diagnostic = cx.lint(it.lint, node, span, it.message);
        for (n, at) in awaits.into_iter().enumerate() {
            let what = if n == 0 { "this `.await` suspends with it held" } else { "and this one" };
            diagnostic.span_note(at, what);
        }
        diagnostic.help(match it.lint {
            Implemented::AwaitHoldingLock => {
                "drop the guard before the `.await` (end its scope with a block, or `drop` it), or use a lock \
                 made to be held across one; a thread that polls another future meanwhile can deadlock on it"
            }
            _ => {
                "end the borrow before the `.await` (end its scope with a block, or `drop` it); another \
                 future that borrows the cell meanwhile panics"
            }
        });
    }
}

fn find<'t>(cx: &Cx<'_, 't>, function: ast::Fn<'t>) -> Vec<Held> {
    // alloc: the values found in one function, reported at the edge
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
    let kinds = [(Kind::Lock, Implemented::AwaitHoldingLock), (Kind::Cell, Implemented::AwaitHoldingRefcellRef)];
    let function_name = guards::function_name(cx, analysis);
    for body in guards::bodies(db, analysis, def) {
        if !body.coroutine {
            continue;
        }
        let mir = body.mir;
        let held = blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |frame| mir.held_locals_in(frame));
        // The block that yields at each yield point and its `.await`, by the yield's index.
        let awaits: Vec<Option<(BasicBlockId, Option<TextRange>)>> = (held.yields.iter()) // alloc: one per yield of one body
            .map(|point| {
                let (id, span) = mir.basic_blocks.iter().find_map(|(id, block)| match &block.terminator {
                    Some(it) if matches!(it.kind, TerminatorKind::Yield { resume, .. } if resume == point.resume) => {
                        Some((id, it.span))
                    }
                    _ => None,
                })?;
                let range = match span {
                    MirSpan::Unknown => None,
                    it => guards::range_of(analysis, it),
                };
                Some((id, range))
            })
            .collect();
        for (at, &local) in held.locals.iter().enumerate() {
            let ty = analysis.ty(mir.locals[local].ty.as_ref());
            // The coroutine layout can keep a local whose value was moved out before the yield,
            // as it keeps its storage; such a local holds no guard there.
            let init_in = guards::maybe_init(mir, local);
            // alloc: a flag per yield of one body
            let holding: Vec<bool> = (held.yields.iter().zip(&awaits))
                .map(|(point, yielded)| {
                    point.kept.contains(&at)
                        && yielded.is_some_and(|(id, _)| {
                            guards::init_before_terminator(mir, id, local, init_in[guards::block_index(id)])
                        })
                })
                .collect();
            if !holding.contains(&true) {
                continue;
            }
            for (kind, lint) in kinds {
                if !cx.on(lint) {
                    continue;
                }
                let Some(guard) = guards.in_ty(db, &ty, kind) else { continue };
                let Some(range) = guards::first_assigned(mir, local).and_then(|it| guards::range_of(analysis, it))
                else {
                    continue;
                };
                // alloc: the `.await`s of one finding, a few
                let mut across: Vec<TextRange> = (holding.iter().zip(&awaits))
                    .filter(|(holding, _)| **holding)
                    .filter_map(|(_, yielded)| yielded.and_then(|(_, range)| range))
                    .collect();
                across.sort_by_key(|it| (it.start(), it.end()));
                across.dedup();
                across.truncate(NOTED);
                let owner = match body.async_fn {
                    false => format!("an `async` block in {function_name}"),
                    true => function_name.clone(), // clone: a message's part
                };
                let itself = ty.as_adt().is_some_and(|adt| guards.kind(db, adt) == Some(kind));
                let inside = match itself {
                    true => String::new(),
                    false => {
                        let target = cx.krate().to_display_target(db);
                        format!(" (in its `{}`)", guard.display(db, target))
                    }
                };
                let message = format!(
                    "{} holds {}{inside} across an `.await` in {owner}",
                    guards::say_local(cx, analysis, mir, local),
                    kind.held()
                );
                out.push(Held { lint, at: range, awaits: across, message });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::drops_tests::lint_only;

    const LINTS: &[&str] = &["await_holding_lock", "await_holding_refcell_ref"];
    const FOUND: &str = "across an `.await`";

    #[test]
    fn a_guard_and_a_refcell_borrow_held_across_an_await_are_found() {
        let found = lint_only(
            LINTS,
            r#"
use std::{cell::RefCell, sync::Mutex};
pub async fn tick() {}
pub async fn held(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    tick().await;
    guard.len()
}
pub async fn borrowed(c: &RefCell<Vec<u32>>) -> usize {
    let b = c.borrow_mut();
    tick().await;
    b.len()
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("index.html#await_holding_lock"), "{found}");
        assert!(found.contains("`guard`, a `MutexGuard<"), "{found}");
        assert!(found.contains("holds the lock across an `.await` in `held`"), "{found}");
        assert!(found.contains("index.html#await_holding_refcell_ref"), "{found}");
        assert!(found.contains("holds the `RefCell` borrow across an `.await` in `borrowed`"), "{found}");
        assert!(found.contains("this `.await` suspends with it held"), "{found}");
    }

    /// Dropped in a block before the `.await`, or moved away by `drop`: the front end's
    /// coroutine layout keeps neither, so neither is held. A guard of a `RwLock` is one too,
    /// and a value with a guard in it holds it.
    #[test]
    fn what_the_future_keeps_is_what_is_held() {
        let found = lint_only(
            LINTS,
            r#"
use std::sync::{Mutex, MutexGuard, RwLock};
pub async fn tick() {}
pub struct Holder<'a> { guard: MutexGuard<'a, u32> }
pub async fn scoped(m: &Mutex<Vec<u32>>) -> usize {
    let n = { let guard = m.lock().unwrap(); guard.len() };
    tick().await;
    n
}
pub async fn moved(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    std::mem::drop(guard);
    tick().await;
    n
}
pub async fn read(r: &RwLock<u32>) -> u32 {
    let seen = r.read().unwrap();
    tick().await;
    *seen
}
pub async fn kept(m: &Mutex<u32>) -> u32 {
    let holder = Holder { guard: m.lock().unwrap() };
    tick().await;
    *holder.guard
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("holds the lock across an `.await` in `read`"), "{found}");
        assert!(found.contains("`holder`, a `Holder<"), "{found}");
        assert!(found.contains("holds the lock (in its `MutexGuard<"), "{found}");
    }

    #[test]
    fn an_async_block_a_renamed_lock_apis_guard_and_a_macro_written_body_are_found() {
        let found = lint_only(
            LINTS,
            r#"
mod locks { pub use parking_lot::Mutex as Lock; }
use locks::Lock;
use std::sync::Mutex;
pub async fn tick() {}
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub fn spawn(m: &Mutex<Vec<u32>>) {
    let _f = async {
        let guard = m.lock().unwrap();
        tick().await;
        guard.len()
    };
}
pub async fn parked(m: &Lock<u32>) -> u32 {
    let held = m.lock();
    tick().await;
    *held
}
item! {
    pub async fn handed(m: &Mutex<u32>) -> u32 {
        let guard = m.lock().unwrap();
        tick().await;
        *guard
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("across an `.await` in an `async` block in `spawn`"), "{found}");
        assert!(found.contains("across an `.await` in `parked`"), "{found}");
        assert!(found.contains("across an `.await` in `handed`"), "{found}");
    }
}
