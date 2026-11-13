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
