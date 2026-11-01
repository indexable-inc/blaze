//! `unnamed_wait` over [`super`]'s fixtures.

use super::check_in;

const CRATES: &str = "//- /src/lib.rs crate:main deps:engine,std,blaze_graph\n";

/// A flight joined by its key's hash, a naming closure that writes nothing or a placeholder
/// (called as a method or by its path), a `wait::wait`, `lock` or `wait_until` named by a
/// placeholder (through a renamed module), `sched::wait` and `EventWait::wait`, and a `Condvar`
/// wait on a driver (and a macro's flight) are findings; the same named, and a scheduler wait
/// inside a `wait::wait`'s block, are none.
#[test]
fn unnamed_wait() {
    let out = check_in(
        CRATES,
        r#"
use blaze_graph::pump::Flights;
use blaze_graph::sched::{self, EventWait, Wait};
use blaze_graph::wait::{self as waits, WaitOn};
use std::sync::{Condvar, Mutex};

pub fn flights(f: &Flights) -> u32 {
    let a = f.get_or_run(1, || 2); //~ unnamed_wait
    let b = f.get_or_run_named(1, |it| it.push_str("variance"), || 2);
    let c = f.get_or_run_named(1, |_| {}, || 2); //~ unnamed_wait
    let d = f.get_or_run_named(1, |it| it.push_str("todo"), || 2); //~ unnamed_wait
    let e = Flights::get_or_run_named(f, 1, |_| {}, || 2); //~ unnamed_wait
    let _ = f.hold(1); //~ unnamed_wait
    let _ = f.hold_named(1, |it| it.push_str("the plan's lock"));
    a + b + c + d + e
}

pub fn named_waits(lock: &Mutex<u32>, ready: &Condvar, event: EventWait) {
    let _ = waits::wait(WaitOn::Key("todo"), None, || 1); //~ unnamed_wait
    let _ = waits::wait(WaitOn::Key("variance of `Foo`"), None, || 1);
    let _ = waits::lock(lock, "wait"); //~ unnamed_wait
    let _ = waits::lock(lock, "the plan");
    let held = lock.lock();
    let held = waits::wait_until(ready, held, "", |_| true); //~ unnamed_wait
    let _held = waits::wait_until(ready, held, "the plan's jobs", |_| true);
    let _ = sched::wait(Wait::Until(1)); //~ unnamed_wait
    let _ = waits::wait(WaitOn::Channel("the plan's jobs"), None, || sched::wait(Wait::Key(1)));
    let _ = event.wait(); //~ unnamed_wait
}

pub fn driver(lock: &Mutex<u32>, ready: &Condvar) {
    let guard = lock.lock();
    let _guard = ready.wait(guard); //~ unnamed_wait
}

macro_rules! fetch {
    ($f:expr) => {
        $f.get_or_run(1, || 2)
    };
}

pub fn by_macro(f: &Flights) -> u32 {
    fetch!(f) //~ unnamed_wait
}
"#,
        "-A clippy::all --warn=blaze::unnamed_wait",
    );
    assert!(out.contains("`Flights::get_or_run` names what it waits for only as its key's hash"), "{out}");
    assert!(out.contains("`Flights::get_or_run_named` names nothing it waits for"), "{out}");
    assert!(out.contains("`wait::lock` names what it waits for only as `\"wait\"`"), "{out}");
    assert!(out.contains("`sched::wait` names nothing it waits for"), "{out}");
    assert!(out.contains("`Condvar::wait` names nothing it waits for"), "{out}");
}
