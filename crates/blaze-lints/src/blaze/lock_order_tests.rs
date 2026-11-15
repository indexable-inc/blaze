//! `lock_order` and `lock_across_wait` over the effects tests' toolchain, whose `MutexGuard`
//! has no `Drop`: a guard is held while it is live, so each fixture keeps its guards with
//! `keep` after the call it should be held across. Per lint, a crate refused with both chains
//! (or the chain to the wait) named and a crate that passes.

use super::effects_tests::{MINICORE, TOOLCHAIN};
use crate::tests::lint;

/// What the two lock lints say of `main` (`src/lib.rs`), after the `keep` every fixture uses.
fn locks_of(main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\npub fn keep<A, B>(_a: &A, _b: &B) {{}}\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, "-A clippy::all --deny=blaze::lock_order --deny=blaze::lock_across_wait")
}

/// The lines of `out` containing `marker`.
fn lines<'o>(out: &'o str, marker: &str) -> Vec<&'o str> {
    out.lines().filter(|line| line.contains(marker)).collect()
}

const CYCLE: &str = "the locks are taken in a cycle";
const ACROSS: &str = "is held across";
/// What a refusal of an unknown says, only at a function that asked for its locks.
const ASKED: &str = "asked for its locks to be proved";

/// An `Arc` for the toolchain's `std`, whose `deref` a lock's class is followed through.
const ARC: &str = r#"pub mod arc {
    pub struct Arc<T>(T);
    impl<T> Arc<T> {
        pub fn new(value: T) -> Arc<T> {
            Arc(value)
        }
    }
    impl<T> core::ops::Deref for Arc<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }
}
pub mod env {"#;

/// `blaze_core::lock`'s `Lock`, made with a name, its acquire at the path its row names.
const BLAZE_CORE: &str = r#"//- /blaze_core/lib.rs crate:blaze_core deps:std
pub mod lock {
    pub struct Lock<T> {
        name: &'static str,
        value: T,
    }
    pub struct LockGuard<'a, T>(&'a Lock<T>);
    impl<T> Lock<T> {
        pub const fn new(name: &'static str, value: T) -> Lock<T> {
            Lock { name, value }
        }
        pub fn lock(&self) -> LockGuard<'_, T> {
            LockGuard(self)
        }
        pub fn try_lock_for(&self, _nanos: u64) -> Option<LockGuard<'_, T>> {
            None
        }
    }
}
"#;

/// [`locks_of`] with [`ARC`] in `std` and [`BLAZE_CORE`] beside it.
fn locks_with(main: &str) -> String {
    let minicore = MINICORE.replace("allocator_api", "allocator_api, deref");
    let toolchain = TOOLCHAIN.replace("pub mod env {", ARC);
    assert_ne!(toolchain, TOOLCHAIN, "the toolchain has the `env` module `ARC` is put before");
    let fixture = format!(
        "{minicore}{toolchain}{BLAZE_CORE}//- /src/lib.rs crate:main deps:alloc,std,blaze_core\npub fn keep<A, B>(_a: &A, _b: &B) {{}}\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, "-A clippy::all --deny=blaze::lock_order --deny=blaze::lock_across_wait")
}

// what the check can't see

/// `broken`'s body does not lower (a type error), so what `calls` takes under it is unknown.
/// Unasked, that is silent: neither lint says anything of `calls` or `broken`.
///
/// The invariant: an unknown is refused only where it was asked for; a function that did not
/// ask gets no finding from what the check can't see.
#[test]
fn an_unknown_is_silent_where_nothing_asked() {
    let out = locks_of(
        r#"
fn broken() -> u32 {
    let v: u32 = "text";
    v
}
pub fn calls(m: &std::sync::Mutex<u32>) -> u32 {
    let guard = m.lock();
    keep(&guard, &guard);
    broken()
}
"#,
    );
    assert_eq!(lines(&out, ASKED), Vec::<&str>::new(), "{out}");
    assert_eq!(lines(&out, CYCLE), Vec::<&str>::new(), "{out}");
    assert_eq!(lines(&out, ACROSS), Vec::<&str>::new(), "{out}");
}

/// The same function marked `#[blaze::lock_checked]` asked: each lint refuses it, once, as
/// unable to show the absence of what it looks for.
#[test]
fn an_unknown_is_refused_where_asked() {
    let out = locks_of(
        r#"
fn broken() -> u32 {
    let v: u32 = "text";
    v
}
#[blaze::lock_checked]
pub fn calls(m: &std::sync::Mutex<u32>) -> u32 {
    let guard = m.lock();
    keep(&guard, &guard);
    broken()
}
"#,
    );
    let found = lines(&out, ASKED);
    assert_eq!(found.len(), 2, "{out}");
    assert!(found.iter().all(|it| it.contains("calls`")), "{out}");
    assert!(found.iter().any(|it| it.contains("lock-order cycle")), "{out}");
    assert!(found.iter().any(|it| it.contains("held across a call that blocks")), "{out}");
}

/// `#![blaze::lock_checked]` on a module asks for every function in it; one outside is silent.
#[test]
fn a_module_asks_for_its_functions() {
    let out = locks_of(
        r#"
fn broken() -> u32 {
    let v: u32 = "text";
    v
}
pub mod checked {
    #![blaze::lock_checked]
    pub fn inside() -> u32 {
        super::broken()
    }
}
pub fn outside() -> u32 {
    broken()
}
"#,
    );
    let found = lines(&out, ASKED);
    assert_eq!(found.len(), 2, "{out}");
    assert!(found.iter().all(|it| it.contains("inside`")), "{out}");
}

// classes

/// Two `Arc<Mutex<_>>` fields of one type are two classes, `Pair.a` and `Pair.b`, through
/// `Arc`'s `deref`: the two orders are a cycle. Known only by type, they were one class and an
/// edge to itself, which is not kept, so the cycle went unseen.
#[test]
fn arc_fields_of_one_type_are_two_classes() {
    let out = locks_with(
        r#"
use std::{arc::Arc, sync::Mutex};
pub struct Pair {
    a: Arc<Mutex<u32>>,
    b: Arc<Mutex<u32>>,
}
impl Pair {
    pub fn ab(&self) {
        let ga = self.a.lock();
        let gb = self.b.lock();
        keep(&ga, &gb);
    }
    pub fn ba(&self) {
        let gb = self.b.lock();
        let ga = self.a.lock();
        keep(&gb, &ga);
    }
}
"#,
    );
    let found = lines(&out, CYCLE);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("Pair.a") && found[0].contains("Pair.b"), "{out}");
}

/// Locks made in the body with a name are classed by it: `"first"` and `"second"`, taken in
/// both orders by two functions, are a cycle.
#[test]
fn named_locks_are_classed_by_name() {
    let out = locks_with(
        r#"
use blaze_core::lock::Lock;
pub fn ab() {
    let first = Lock::new("first", 0u32);
    let second = Lock::new("second", 0u32);
    let ga = first.lock();
    let gb = second.lock();
    keep(&ga, &gb);
}
pub fn ba() {
    let first = Lock::new("first", 0u32);
    let second = Lock::new("second", 0u32);
    let gb = second.lock();
    let ga = first.lock();
    keep(&gb, &ga);
}
"#,
    );
    let found = lines(&out, CYCLE);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("\"first\"") && found[0].contains("\"second\""), "{out}");
}

/// Two accessors of locks handed in as parameters, known only by type, are classed by their
/// paths at the caller, `first()` and `second()`: the two orders are a cycle.
#[test]
fn accessors_of_one_type_are_classed_by_path() {
    let out = locks_of(
        r#"
use std::sync::{Mutex, MutexGuard};
pub fn first(m: &Mutex<u32>) -> MutexGuard<'_, u32> {
    m.lock()
}
pub fn second(m: &Mutex<u32>) -> MutexGuard<'_, u32> {
    m.lock()
}
pub fn ab(x: &Mutex<u32>, y: &Mutex<u32>) {
    let a = first(x);
    let b = second(y);
    keep(&a, &b);
}
pub fn ba(x: &Mutex<u32>, y: &Mutex<u32>) {
    let b = second(y);
    let a = first(x);
    keep(&b, &a);
}
"#,
    );
    let found = lines(&out, CYCLE);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("first()") && found[0].contains("second()"), "{out}");
}

/// Two parameters of one lock type stay one class: no cycle is claimed between them (what still
/// collapses, said in `hir_ty_pure::locks`).
#[test]
fn parameters_of_one_type_stay_one_class() {
    let out = locks_of(
        r#"
pub fn ab(x: &std::sync::Mutex<u32>, y: &std::sync::Mutex<u32>) {
    let a = x.lock();
    let b = y.lock();
    keep(&a, &b);
}
pub fn ba(x: &std::sync::Mutex<u32>, y: &std::sync::Mutex<u32>) {
    let b = y.lock();
    let a = x.lock();
    keep(&b, &a);
}
"#,
    );
    assert_eq!(lines(&out, CYCLE), Vec::<&str>::new(), "{out}");
}

// lock_order

/// `ab` holds `Pair.a` while taking `Pair.b`, `ba` the other way: one finding, at `ab` (whose
/// edge is the least of the cycle), naming both classes and both functions.
#[test]
fn lock_order_refuses_two_orders() {
    let out = locks_of(
        r#"
pub struct Pair {
    a: std::sync::Mutex<u32>,
    b: std::sync::Mutex<u32>,
}
impl Pair {
    pub fn ab(&self) {
        let ga = self.a.lock();
        let gb = self.b.lock();
        keep(&ga, &gb);
    }
    pub fn ba(&self) {
        let gb = self.b.lock();
        let ga = self.a.lock();
        keep(&gb, &ga);
    }
}
"#,
    );
    let found = lines(&out, CYCLE);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("Pair.a") && found[0].contains("Pair.b"), "{out}");
    assert!(found[0].contains("ab`") && found[0].contains("ba`"), "{out}");
}

/// The second lock taken in a callee still makes the edge: `outer` holds `Pair.b` and calls
/// `take_a`, against `ab`'s order. The chain names the callee.
#[test]
fn lock_order_follows_a_callee() {
    let out = locks_of(
        r#"
pub struct Pair {
    a: std::sync::Mutex<u32>,
    b: std::sync::Mutex<u32>,
}
impl Pair {
    pub fn ab(&self) {
        let ga = self.a.lock();
        let gb = self.b.lock();
        keep(&ga, &gb);
    }
    fn take_a(&self) {
        let ga = self.a.lock();
        keep(&ga, &ga);
    }
    pub fn outer(&self) {
        let gb = self.b.lock();
        self.take_a();
        keep(&gb, &gb);
    }
}
"#,
    );
    let found = lines(&out, CYCLE);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("take_a`"), "{out}");
}

/// Both functions take `Pair.a` first: no cycle.
#[test]
fn lock_order_passes_one_order() {
    let out = locks_of(
        r#"
pub struct Pair {
    a: std::sync::Mutex<u32>,
    b: std::sync::Mutex<u32>,
}
impl Pair {
    pub fn ab(&self) {
        let ga = self.a.lock();
        let gb = self.b.lock();
        keep(&ga, &gb);
    }
    pub fn again(&self) {
        let ga = self.a.lock();
        let gb = self.b.lock();
        keep(&gb, &ga);
    }
}
"#,
    );
    assert_eq!(lines(&out, CYCLE), Vec::<&str>::new(), "{out}");
}

// lock_across_wait

/// A guard live across a syscall that waits (`read`) is refused, naming the lock and the call.
#[test]
fn lock_across_wait_refuses_a_blocking_call() {
    let out = locks_of(
        r#"
unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
}
pub struct Inbox {
    state: std::sync::Mutex<u32>,
}
pub fn input(inbox: &Inbox, buf: &mut [u8; 8]) -> isize {
    let guard = inbox.state.lock();
    let n = unsafe { read(0, buf as *mut [u8; 8] as *mut u8, 8) };
    keep(&guard, &n);
    n
}
"#,
    );
    let found = lines(&out, ACROSS);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("Inbox.state"), "{out}");
    assert!(found[0].contains("`read`"), "{out}");
}

/// The guard's last use is before the wait: it is not held across it.
#[test]
fn lock_across_wait_passes_a_guard_done_before_the_call() {
    let out = locks_of(
        r#"
unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
}
pub struct Inbox {
    state: std::sync::Mutex<u32>,
}
pub fn input(inbox: &Inbox, buf: &mut [u8; 8]) -> isize {
    let guard = inbox.state.lock();
    keep(&guard, &guard);
    unsafe { read(0, buf as *mut [u8; 8] as *mut u8, 8) }
}
"#,
    );
    assert_eq!(lines(&out, ACROSS), Vec::<&str>::new(), "{out}");
}
