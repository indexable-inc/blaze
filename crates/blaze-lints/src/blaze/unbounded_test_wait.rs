//! `unbounded_test_wait`: a test that waits with no deadline, so that when what it waits for
//! hangs, the test hangs instead of failing.
//!
//! The rule: every wait in a test has a deadline and fails naming what it waited for (rule
//! `bounded-progress`). What a test waits on is the code under test, and the code under test
//! is what may be broken: a test sat ten hours in `thread::park` with nothing to say what it
//! waited for, holding the run.
//!
//! Test code is a `#[test]` function, anything under `#[cfg(test)]`, and every body of an
//! integration test's crate (`tests/*.rs`, helpers too). A finding there is a call of:
//!
//! - what `unbounded_wait` finds outside tests: `std::thread::park`, a `Condvar` (std's,
//!   `parking_lot`'s) or `blaze_core::lock::Signal` `wait` or `wait_while`, a function of
//!   `blaze_core::wait` given `None` for its deadline, written out;
//! - a channel's `Receiver::recv` (std's, `crossbeam_channel`'s);
//! - `std::process::Child::wait` or `wait_with_output`, unless the same child was killed
//!   earlier in the same block (a killed child ends);
//!
//! or a `loop` or `while` loop around `Child::try_wait` that neither sleeps nor waits (a loop
//! that does is `open_wait_loop`'s, in tests too) and compares no deadline or round count where
//! it ends, by a `break`, a `return` or a panic.
//!
//! This is `unbounded_wait`'s twin rather than that lint widened, because the two judge the
//! same call differently. Outside a test a `recv` or a `join` is ended by the other side's end
//! (the last sender dropped, the thread returned), and a `// wakes: <who>` comment can say
//! what always wakes a park. In a test the other side is the code under test, whose bug is
//! the hang being looked for, so neither answer holds: there is no `// wakes:` exemption.
//!
//! Not found, each with why:
//!
//! - `JoinHandle::join`: a joined thread hangs only in a wait of its own, which is found where
//!   it is written (a test's thread) or by `unbounded_wait` (the code under test's). Every join
//!   would be about 450 findings in the tree, nearly all of threads that only compute.
//! - `Barrier::wait`: `.clippy.toml` bans `std::sync::Barrier`; the tests that keep one use it
//!   as a start gate, which nothing can fail to reach.
//! - `Command::output` and `status`: in the tree's tests they mostly run `git`, `cc` or `kill`,
//!   tools that end; a test that runs the program under test spawns it and waits through
//!   `stdx::child`.
//! - a loop around `try_recv`: as often a drain (`while let Ok(it) = rx.try_recv()`) as a wait,
//!   and one that waits sleeps or yields, which `open_wait_loop` finds.
//! - a deadline that is `None` only at run time, and a `Child::wait` after a bounded wait saw
//!   the child exit (`ended_within`, `wait::exited`): name resolution sees the call, not the
//!   order of events; such a reap carries an `#[expect]` saying what bounded it.

use base_db::{Db, TargetKind};
use hir_def::{
    FunctionId,
    expr_store::Body,
    hir::{Expr, ExprId, LoopSource},
    signatures::FunctionSignature,
};

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
    open_wait_loop, unbounded_wait,
};
use crate::check::Cx;

const LINT: Lint = Lint::UnboundedTestWait;

/// How many expressions up a call is followed to its block.
const UP: usize = 64;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether the body walked is test code: inside a `#[test]` function or under `#[cfg(test)]`
/// ([`Cx::is_in_test`]), or of an integration test's crate, whose every body only the test
/// harness runs.
pub(super) fn in_test(cx: &Cx<'_, '_>) -> bool {
    cx.is_in_test(cx.root()) || matches!(cx.krate().target_kind(cx.db()), TargetKind::Test)
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    if !in_test(cx) {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let (at, message) = match expr {
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                let Some((f, _)) = written_callee(infer, body, id) else { continue };
                let what = match unbounded_wait::unbounded(db, f) {
                    Some(what) => what,
                    None if unbounded_wait::in_wait(db, hir::Function::from(f).module(db))
                        && unbounded_wait::deadline_is_none(cx, analysis, body, id) =>
                    {
                        // alloc: a message's part
                        format!("`{}` given `None` for its deadline", unbounded_wait::name_of(db, f))
                    }
                    None => match blocking(db, f) {
                        Some(Blocks::Child(what)) => {
                            let parents = parents.get_or_insert_with(|| Parents::of(body));
                            if killed_before(cx, analysis, parents, id) {
                                continue;
                            }
                            what.to_owned() // alloc: a message's part
                        }
                        Some(Blocks::Channel(what)) => what.to_owned(), // alloc: a message's part
                        None => continue,
                    },
                };
                // alloc: reported at the edge
                (id, format!("{what} waits in a test with no deadline: when what it waits for hangs, the test hangs"))
            }
            Expr::Loop { body: inner, label, source } => {
                let kind = match source {
                    LoopSource::Loop => "`loop`",
                    LoopSource::While => "`while` loop",
                    _ => continue,
                };
                let Some(spin) = first_try_wait(db, body, analysis, inner) else { continue };
                if open_wait_loop::first_wait(cx, analysis, inner).is_some() {
                    continue;
                }
                let parents = parents.get_or_insert_with(|| Parents::of(body));
                let exits = open_wait_loop::exits(body, parents, id, inner, label, source);
                let panics = open_wait_loop::panics(analysis, parents, id, inner);
                if (exits.is_empty() && panics.is_empty())
                    || exits
                        .iter()
                        .chain(&panics)
                        .any(|&guard| guard.is_some_and(|it| open_wait_loop::compares(analysis, it)))
                {
                    continue;
                }
                // alloc: reported at the edge
                (spin, format!(
                    "this {kind} in a test waits for `Child::try_wait` with nothing bounding it: no deadline or \
                     round count is compared where it ends"
                ))
            }
            _ => continue,
        };
        let Some(range) = analysis.expr_range(at) else { continue };
        let note = "a test waits on the code under test, which may be what hangs: a wait with no deadline holds \
                    the run with nothing said (a test sat ten hours in `thread::park`); wait to a deadline and \
                    fail naming what was awaited";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: reported at the edge
    }
    found
}

/// A call that blocks with no deadline whatever it is given, beside `unbounded_wait`'s.
enum Blocks {
    /// A wait for a child process to exit.
    Child(&'static str),
    /// A wait for a channel's next message.
    Channel(&'static str),
}

/// What a call of `f` is, when it is a wait [`Blocks`] names.
fn blocking(db: &Db, f: FunctionId) -> Option<Blocks> {
    let adt = facts::impl_adt(db, f)?;
    let krate = adt.module(db).krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    match (adt.name(db).as_str(), name) {
        ("Receiver", "recv") if is("std") || is("crossbeam_channel") => {
            Some(Blocks::Channel("`Receiver::recv`"))
        }
        ("Child", "wait") if is("std") => Some(Blocks::Child("`Child::wait`")),
        ("Child", "wait_with_output") if is("std") => {
            Some(Blocks::Child("`Child::wait_with_output`"))
        }
        _ => None,
    }
}

/// Whether `f` is `std::process::Child`'s method `name`.
fn is_child(db: &Db, f: FunctionId, name: &str) -> bool {
    FunctionSignature::of(db, f).name.as_str() == name
        && facts::impl_adt(db, f).is_some_and(|adt| facts::adt_is(db, adt, "std", "Child"))
}

/// Whether the child the method call `wait` waits for was killed earlier in the block the call
/// is in: a `kill` of the same receiver, as written, before it.
fn killed_before(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    parents: &Parents,
    wait: ExprId,
) -> bool {
    let body = analysis.body();
    let infer = analysis.infer();
    let db = cx.db();
    let Expr::MethodCall { receiver, .. } = body.expr(wait) else { return false };
    let (Some(child), Some(at)) = (analysis.expr_range(receiver), analysis.expr_range(wait)) else {
        return false;
    };
    let child = cx.snippet(child);
    let Some(block) = parents.up(wait, UP).find(|&it| matches!(body.expr(it), Expr::Block { .. }))
    else {
        return false;
    };
    let mut stack = vec![block]; // alloc: the expressions of one block
    while let Some(id) = stack.pop() {
        match body.expr(id) {
            Expr::Closure { .. } => continue,
            Expr::MethodCall { receiver: killed, .. }
                if written_callee(infer, body, id).is_some_and(|(f, _)| is_child(db, f, "kill"))
                    && analysis.expr_range(id).is_some_and(|it| it.end() <= at.start())
                    && analysis.expr_range(killed).is_some_and(|it| cx.snippet(it) == child) =>
            {
                return true;
            }
            _ => {}
        }
        body.walk_child_exprs(id, |it| stack.push(it));
    }
    false
}

/// The first call of `Child::try_wait` in the loop body `inner`, outside the loops and
/// closures in it.
fn first_try_wait(
    db: &Db,
    body: &Body,
    analysis: &hir::BodyAnalysis<'_>,
    inner: ExprId,
) -> Option<ExprId> {
    let infer = analysis.infer();
    let mut stack = vec![inner]; // alloc: the expressions of one loop body
    // alloc: the calls of one loop body, a few
    let mut calls = Vec::new();
    while let Some(id) = stack.pop() {
        match body.expr(id) {
            Expr::Loop { .. } | Expr::Closure { .. } => continue,
            Expr::Call { .. } | Expr::MethodCall { .. }
                if written_callee(infer, body, id)
                    .is_some_and(|(f, _)| is_child(db, f, "try_wait")) =>
            {
                calls.push(id);
            }
            _ => {}
        }
        body.walk_child_exprs(id, |child| stack.push(child));
    }
    calls.into_iter().min_by_key(|&id| analysis.expr_range(id).map(|it| it.start()))
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "waits in a test with no deadline";
    const LOOP: &str = "with nothing bounding it";

    /// An `std` with the waits a test makes (a thread park, a `Condvar`, a channel, a child
    /// process), a `crossbeam_channel`, and a `blaze_core` with `lock::Signal` and
    /// `wait::exited`, by the paths the real ones have.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn, panic
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
    pub fn sleep(_nanos: u64) {}
    pub struct JoinHandle(u32);
    impl JoinHandle {
        pub fn join(self) {}
    }
}
pub mod time {
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant { Instant(0) }
        pub fn elapsed(&self) -> u64 { self.0 }
    }
}
pub mod process {
    pub struct ExitStatus(i32);
    pub struct Output(i32);
    pub struct Child(u32);
    impl Child {
        pub fn wait(&mut self) -> Result<ExitStatus, ()> { Ok(ExitStatus(0)) }
        pub fn wait_with_output(self) -> Result<Output, ()> { Ok(Output(0)) }
        pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ()> { Ok(None) }
        pub fn kill(&mut self) -> Result<(), ()> { Ok(()) }
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
            pub fn recv(&self) -> Result<u32, ()> { Err(()) }
            pub fn recv_timeout(&self, _nanos: u64) -> Result<u32, ()> { Err(()) }
        }
    }
}
//- /crossbeam_channel/lib.rs crate:crossbeam_channel deps:std
pub struct Receiver(u32);
impl Receiver {
    pub fn recv(&self) -> Result<u32, ()> { Err(()) }
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
    pub fn exited(_what: &'static str, _pid: i32, _deadline: Option<Deadline>) -> Result<Waited, ()> {
        Ok(Waited::Ready)
    }
}
//- /src/lib.rs crate:main deps:std,crossbeam_channel,blaze_core cfg:test
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::unbounded_test_wait")
    }

    /// The bug in miniature: a test that parks until the code under test unparks it, and the
    /// other waits a test makes on what it runs, each with no deadline.
    #[test]
    fn a_test_that_waits_with_no_deadline_is_found() {
        let out = found(
            r#"
#[cfg(test)]
mod tests {
    use std::{process::Child, sync::{Condvar, Guard, mpsc::Receiver}};
    use blaze_core::{lock::{LockGuard, Signal}, wait};
    fn parks() {
        std::thread::park(); // wakes: the worker, which is what the test is about
    }
    fn conditions(ready: &Condvar, guard: Guard, signal: &Signal, held: &mut LockGuard) {
        let _guard = ready.wait(guard);
        signal.wait(held);
    }
    fn channels(rx: &Receiver, other: &crossbeam_channel::Receiver) {
        let _ = rx.recv();
        let _ = other.recv();
    }
    fn children(mut child: Child, other: Child, pid: i32) {
        let _ = child.wait();
        let _ = other.wait_with_output();
        let _ = wait::exited("the child", pid, None);
    }
    fn spins(child: &mut Child) {
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 8, "{out}");
        assert_eq!(out.matches(LOOP).count(), 1, "{out}");
        assert!(out.contains("`std::thread::park` waits in a test"), "a `// wakes:` is no answer in a test: {out}");
        assert!(out.contains("`Condvar::wait` waits in a test"), "{out}");
        assert!(out.contains("`Signal::wait` waits in a test"), "{out}");
        assert_eq!(out.matches("`Receiver::recv` waits in a test").count(), 2, "{out}");
        assert!(out.contains("`Child::wait` waits in a test"), "{out}");
        assert!(out.contains("`Child::wait_with_output` waits in a test"), "{out}");
        assert!(out.contains("`wait::exited` given `None` for its deadline waits in a test"), "{out}");
        assert!(out.contains("this `loop` in a test waits for `Child::try_wait`"), "{out}");
        assert!(out.contains("ten hours in `thread::park`"), "{out}");
    }

    /// The fix, and what is left alone: the same waits to a deadline, a reap after a kill, a
    /// `try_wait` loop that compares a deadline or panics past one, a join, and the same
    /// unbounded waits outside test code.
    #[test]
    fn a_bounded_test_wait_a_reap_after_a_kill_and_code_outside_tests_are_left_alone() {
        let out = found(
            r#"
use std::{process::Child, sync::mpsc::Receiver};
pub fn served(rx: &Receiver, mut child: Child) {
    let _ = rx.recv();
    let _ = child.wait();
    std::thread::park();
}
#[cfg(test)]
mod tests {
    use std::{process::Child, sync::{Condvar, Guard, mpsc::Receiver}, thread::JoinHandle, time::Instant};
    use blaze_core::{lock::{LockGuard, Signal}, wait::{self, Deadline}};
    fn timed(rx: &Receiver, ready: &Condvar, guard: Guard, signal: &Signal, held: &mut LockGuard, pid: i32) {
        let _ = rx.recv_timeout(5);
        let _guard = ready.wait_timeout(guard, 5);
        let _ = signal.wait_for(held, 5);
        std::thread::park_timeout(5);
        let _ = wait::exited("the child", pid, Some(Deadline(5)));
    }
    fn reaped(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
    fn deadline(child: &mut Child) {
        let began = Instant::now();
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if began.elapsed() > 5 {
                core::panic!("the child did not exit");
            }
        }
    }
    fn counted(child: &mut Child) -> bool {
        let mut rounds = 0;
        while let Ok(None) = child.try_wait() {
            if rounds > 8 {
                return false;
            }
            rounds += 1;
        }
        true
    }
    fn sleeps(child: &mut Child) {
        while let Ok(None) = child.try_wait() {
            std::thread::sleep(1);
        }
    }
    fn joined(worker: JoinHandle) {
        worker.join();
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert_eq!(out.matches(LOOP).count(), 0, "a loop that sleeps is `open_wait_loop`'s: {out}");
    }

    /// An `#[expect]` saying what bounds a wait the lint cannot see bounded is fulfilled: no
    /// finding, and no unfulfilled expectation.
    #[test]
    fn an_expect_naming_what_bounds_the_wait_is_fulfilled() {
        let out = found(
            r#"
#[cfg(test)]
mod tests {
    use std::process::Child;
    #[expect(blaze::unbounded_test_wait, reason = "reaps a child `wait::exited` saw exit within its deadline")]
    fn reaps(mut child: Child) {
        let _ = child.wait();
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "the expectation is met:\n{out}");
    }
}
