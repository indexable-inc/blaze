//! `unbounded_wait`: a thread parked, or a condition waited for, with no deadline, and no
//! `// wakes: <who>` comment on its line or the line above naming what always wakes it.
//!
//! The rule: a wait has a deadline, or its line names the waker and why the waker cannot end
//! without waking it (rules `deadline-waits` and `bounded-progress`). A wait with neither hangs
//! forever the day its waker ends another way: inside the build-script sandbox the workers'
//! wait ring was refused, each worker returned from its error path, and the main thread stayed
//! in `thread::park()`.
//!
//! A finding is a call of:
//!
//! - `std::thread::park`;
//! - `Condvar::wait` or `wait_while` (std's, `parking_lot`'s), or `blaze_core::lock::Signal`'s
//!   `wait` or `wait_while`;
//! - a function of `blaze_core::wait` (`Parker::park`, `until`, `exited`, `readable`,
//!   `pollable`) given `None` for its deadline, written out.
//!
//! Not found: a channel's `recv` and a thread's `join`, which the other side's end wakes (a
//! sender dropped, a thread returned), so a waker that ends silently still ends the wait; a
//! deadline that is `None` only at run time; a raw kernel wait, which `.clippy.toml` bans
//! outside `crates/blaze-core/src/wait*`. `blaze_core::wait` itself, where the deadline is
//! applied, and tests are not linted. There is no fix: the deadline and what to do when it
//! passes are the caller's.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _,
    expr_store::Body,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};

use super::{Finding, Lint, alloc::written_callee, exempt, facts};
use crate::check::Cx;

const LINT: Lint = Lint::UnboundedWait;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if in_wait(db, hir::Module::from(def.module(db))) || cx.is_in_test(cx.root()) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, _)) = written_callee(infer, body, id) else { continue };
        let what = match unbounded(db, f) {
            Some(what) => what,
            None if in_wait(db, hir::Function::from(f).module(db))
                && deadline_is_none(cx, analysis, body, id) =>
            {
                // alloc: a message's part
                format!("`{}` given `None` for its deadline", name_of(db, f))
            }
            None => continue,
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let message = format!(
            "{what} waits with no deadline and no `// wakes: <who>` naming what always wakes it"
        );
        // alloc: a message's part
        let note = "a wait with no deadline hangs forever when its waker ends without waking it (a worker that \
                    returns from an error path, a thread that panics): wait through `blaze_core::wait` to a \
                    `Deadline` and handle `Waited::Deadline` with an error naming what was awaited"
            .to_owned();
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// Whether `module` is `blaze_core::wait` or a module under it: where the one deadline wait is
/// written.
fn in_wait(db: &Db, module: hir::Module) -> bool {
    facts::crate_is(db, module.krate(db), "blaze_core")
        && module
            .path_to_root(db)
            .iter()
            .any(|it| it.name(db).is_some_and(|it| it.as_str() == "wait"))
}

/// `f` as a message names it: with its type when it is a method.
fn name_of(db: &Db, f: FunctionId) -> String {
    let name = FunctionSignature::of(db, f).name.as_str();
    // alloc: a message's part
    match facts::impl_adt(db, f) {
        Some(adt) => format!("{}::{name}", adt.name(db).as_str()),
        None => format!("wait::{name}"),
    }
}

/// What a call of `f` is, when it waits with no deadline whatever it is given.
fn unbounded(db: &Db, f: FunctionId) -> Option<String> {
    let module = hir::Function::from(f).module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    let adt = facts::impl_adt(db, f).map(|it| it.name(db));
    let adt = adt.as_ref().map(hir::Name::as_str);
    let waits = matches!(name, "wait" | "wait_while");
    let found = match adt {
        None => {
            // Under `std::thread`, whichever file of it std keeps the function in.
            name == "park"
                && is("std")
                && module
                    .path_to_root(db)
                    .iter()
                    .any(|it| it.name(db).is_some_and(|it| it.as_str() == "thread"))
        }
        Some("Condvar") => waits && (is("std") || is("parking_lot")),
        Some("Signal") => waits && is("blaze_core"),
        Some(_) => false,
    };
    // alloc: a message's part
    found.then(|| match adt {
        Some(adt) => format!("`{adt}::{name}`"),
        None => "`std::thread::park`".to_owned(),
    })
}

/// Whether an argument of the call `id` is `None`, written out.
fn deadline_is_none(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    body: &Body,
    id: ExprId,
) -> bool {
    let args = match body.expr(id) {
        Expr::Call { args, .. } | Expr::MethodCall { args, .. } => args,
        _ => return false,
    };
    args.iter().any(|&arg| {
        matches!(body.expr(arg), Expr::Path(_))
            && analysis.expr_range(arg).is_some_and(|range| cx.snippet(range) == "None")
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "waits with no deadline and no `// wakes: <who>`";

    /// An `std` with the thread park and a `Condvar`, and a `blaze_core` with `lock::Signal`
    /// and `wait::Parker`, by the paths the real ones have.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod thread {
    pub fn park() {}
    pub fn park_timeout(_nanos: u64) {}
    pub struct JoinHandle(u32);
    impl JoinHandle {
        pub fn join(self) {}
    }
}
pub mod sync {
    pub struct Guard(u32);
    pub struct Condvar;
    impl Condvar {
        pub fn wait(&self, guard: Guard) -> Guard { guard }
        pub fn wait_timeout(&self, guard: Guard, _nanos: u64) -> Guard { guard }
    }
    pub mod mpsc {
        pub struct Receiver(u32);
        impl Receiver {
            pub fn recv(&self) -> Option<u32> { None }
        }
    }
}
//- /blaze_core/lib.rs crate:blaze_core deps:std
pub mod lock {
    pub struct LockGuard(u32);
    pub struct Signal(u32);
    impl Signal {
        pub fn wait(&self, _guard: &mut LockGuard) {}
        pub fn wait_for(&self, _guard: &mut LockGuard, _most: u64) -> bool { true }
    }
}
pub mod wait {
    pub struct Deadline(pub u64);
    pub enum Waited { Ready, Deadline }
    pub struct Parker(u32);
    impl Parker {
        pub fn park(&self, _what: &'static str, _deadline: Option<Deadline>) -> Result<Waited, ()> {
            Ok(Waited::Ready)
        }
    }
    pub fn exited(_what: &'static str, _pid: i32, _deadline: Option<Deadline>) -> Result<Waited, ()> {
        Ok(Waited::Ready)
    }
}
//- /src/lib.rs crate:main deps:std,blaze_core
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::unbounded_wait")
    }

    /// The bug in miniature: a thread that blocks until its workers are done parks with no
    /// bound, so workers that end without unparking it leave it there.
    #[test]
    fn a_park_with_no_bound_and_no_waker_named_is_found() {
        let out = found(
            r#"
use std::thread::park as rest;
use blaze_core::{lock::{LockGuard, Signal}, wait::{self, Parker}};
pub fn block(until: &dyn Fn() -> bool) {
    while !until() {
        std::thread::park();
    }
}
pub fn renamed() {
    rest();
}
pub fn conditions(ready: &std::sync::Condvar, guard: std::sync::Guard, signal: &Signal, held: &mut LockGuard) {
    let _guard = ready.wait(guard);
    signal.wait(held);
}
pub fn forever(parker: &Parker, pid: i32) {
    let _ = parker.park("the pool's idle park", None);
    let _ = wait::exited("a child's exit", pid, None);
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 6, "{out}");
        assert!(out.contains("`std::thread::park` waits with no deadline"), "{out}");
        assert!(out.contains("`Condvar::wait` waits with no deadline"), "{out}");
        assert!(out.contains("`Signal::wait` waits with no deadline"), "{out}");
        assert!(out.contains("`Parker::park` given `None` for its deadline"), "{out}");
        assert!(out.contains("`wait::exited` given `None` for its deadline"), "{out}");
        assert!(out.contains("handle `Waited::Deadline`"), "{out}");
    }

    /// The fix: the same wait to a deadline is not a finding; neither is a wait whose line
    /// names its waker, a timed wait, or a wait the other side's end wakes.
    #[test]
    fn a_deadline_a_named_waker_and_waits_the_other_end_wakes_are_left_alone() {
        let out = found(
            r#"
use blaze_core::{lock::{LockGuard, Signal}, wait::{Deadline, Parker, Waited}};
pub fn block(parker: &Parker, until: &dyn Fn() -> bool, by: u64) -> Result<(), &'static str> {
    while !until() {
        match parker.park("the scope's jobs", Some(Deadline(by))) {
            Ok(Waited::Ready) => {}
            Ok(Waited::Deadline) | Err(()) => return Err("the scope's jobs did not end by the deadline"),
        }
    }
    Ok(())
}
pub fn named(signal: &Signal, held: &mut LockGuard) {
    // wakes: the sampler's owner, on `stop` and on every new entry, under this lock
    signal.wait(held);
    std::thread::park(); // wakes: the pool's `Drop`, which unparks every thread it listed
}
pub fn timed(ready: &std::sync::Condvar, guard: std::sync::Guard, signal: &Signal, held: &mut LockGuard) {
    let _guard = ready.wait_timeout(guard, 5);
    let _ = signal.wait_for(held, 5);
    std::thread::park_timeout(5);
}
pub fn ended_by_the_other_side(rx: &std::sync::mpsc::Receiver, worker: std::thread::JoinHandle) {
    let _ = rx.recv();
    worker.join();
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }
}
