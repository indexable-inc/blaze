//! `#[blaze::may(..)]` over the effects tests' toolchain (`effects_tests`): what a function may
//! do, said positively. Under-declaration is refused for each word with its witness;
//! `diverge` is every loop not known to end (the loop's line), recursion (the chain, and that
//! the function is called again) and a call through a function pointer or an unmarked `dyn`;
//! a counted `while` passes and its near misses do not; over-declaration is advice on a private
//! function only; an impl stays within its trait's method; a module's default is replaced by a
//! function's own list; the old spellings still hold and are rewritten to the list they stand
//! for; and a crate that declares nothing is not looked at.

use super::effects_tests::{MINICORE, TOOLCHAIN};
use crate::tests::lint;

/// What the lints say of `main` (`src/lib.rs`, its crate's meta `meta` after `crate:main`).
fn may_of(meta: &str, main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std{meta}\n{}",
        main.trim_start_matches('\n')
    );
    lint(
        &fixture,
        "-A clippy::all --deny=blaze::undeclared_effect --warn=blaze::unneeded_effect --warn=blaze::negative_effect --deny=blaze::nopanic --deny=blaze::noalloc --deny=blaze::nonblocking",
    )
}

/// The lines of `out` that say `marker`.
fn saying<'o>(out: &'o str, marker: &str) -> Vec<&'o str> {
    out.lines().filter(|line| line.contains(marker)).collect()
}

/// What an `undeclared_effect` finding of an effect outside an allowance says.
const UNDECLARED: &str = "does not allow: it ";
/// What an `unneeded_effect` finding says.
const UNNEEDED: &str = "is proved not to";
/// What a `negative_effect` finding says.
const NEGATIVE: &str = "says what it may not do";
/// What `diverge` is said as.
const DIVERGE: &str = "may not return (`diverge`)";

// Under-declaration, word by word.

/// Straight-line code has no effect: under the gate, with no list anywhere, nothing is found.
#[test]
fn may_passes_straight_line_code() {
    let out = may_of(
        "",
        r#"
#![blaze::effects]
fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
pub fn top(a: u32) -> u32 {
    mix(a, 1)
}
"#,
    );
    assert_eq!(saying(&out, UNDECLARED), Vec::<&str>::new(), "{out}");
}

/// Each of the four effects a list leaves out is refused at the function, naming the effect,
/// the line of what has it, and the word to add.
#[test]
fn an_undeclared_effect_is_refused_naming_the_effect_and_its_witness() {
    let out = may_of(
        "",
        r#"
#[blaze::may(alloc, block, diverge)]
pub fn pick(v: &[u32], i: usize) -> u32 { v[i] }
#[blaze::may(panic, block, diverge)]
pub fn grow() { let _ = alloc::vec::Vec::<u32>::with_capacity(4); }
#[blaze::may(panic, alloc, diverge)]
pub fn wait(m: &std::sync::Mutex<u32>) { let _guard = m.lock(); }
#[blaze::may(panic, alloc, block)]
pub fn spin() { loop {} }
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 4, "{out}");
    let of = |name: &str| {
        *found.iter().find(|it| it.contains(&format!("`fn {name}`"))).unwrap_or_else(|| panic!("no finding of {name}:\n{out}"))
    };
    let pick = of("pick");
    assert!(pick.contains("`fn pick` may panic, which its `#[blaze::may(alloc, block, diverge)]`"), "{out}");
    assert!(pick.contains("add `panic` to its `#[blaze::may(..)]`, or handle it"), "{out}");
    let grow = of("grow");
    assert!(grow.contains("`fn grow` may allocate from the global heap"), "{out}");
    assert!(grow.contains("`main::grow` → "), "the chain to the allocator:\n{out}");
    assert!(grow.contains("add `alloc`"), "{out}");
    let wait = of("wait");
    assert!(wait.contains("`fn wait` may block the thread") && wait.contains("waits for the lock"), "{out}");
    assert!(wait.contains("add `block`"), "{out}");
    let spin = of("spin");
    assert!(spin.contains(&format!("`fn spin` {DIVERGE}")), "{out}");
    assert!(spin.contains("has a loop that is not known to end") && spin.contains("add `diverge`"), "{out}");
    // Each witness is at its line: the index, the lock and the loop.
    for line in ["lib.rs:2", "lib.rs:6", "lib.rs:8"] {
        assert!(out.contains(line), "{line}:\n{out}");
    }
}

/// Under the gate a function with no list may have no effect: the finding says so, and the
/// fix is the list.
#[test]
fn the_gate_gives_an_unmarked_function_no_effect() {
    let out = may_of(
        "",
        r#"
#![blaze::effects]
pub fn pick(v: &[u32], i: usize) -> u32 { v[i] }
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`fn pick` may panic, which `#![blaze::effects]`"), "{out}");
}

/// A crate that declares nothing is not looked at: an unmarked function there may do anything.
#[test]
fn a_crate_that_declares_nothing_is_unaffected() {
    let out = may_of(
        "",
        r#"
pub fn anything(v: &[u32], i: usize, m: &std::sync::Mutex<u32>) -> u32 {
    let _ = alloc::vec::Vec::<u32>::with_capacity(4);
    let _guard = m.lock();
    loop { if v[i] == 0 { return 1; } }
}
#[blaze::nopanic]
pub fn old(a: u32) -> u32 { a ^ 1 }
"#,
    );
    for marker in [UNDECLARED, UNNEEDED, NEGATIVE] {
        assert_eq!(saying(&out, marker), Vec::<&str>::new(), "{out}");
    }
}

// `diverge`: loops.

/// A counted `while` ends: the counter rises (or falls) by one towards a limit the loop does
/// not write, under a strict test, so the step cannot wrap. Nested, each loop on its own.
#[test]
fn a_counted_while_is_known_to_end() {
    let out = may_of(
        "",
        r#"
#[blaze::may()]
pub fn count(n: u32) -> u32 {
    let mut i = 0;
    let mut acc = 0;
    while i < n { acc ^= i; i += 1; }
    acc
}
#[blaze::may()]
pub fn down(n: u32) -> u32 {
    let mut i = n;
    while i > 0 { i -= 1; }
    i
}
#[blaze::may()]
pub fn nested(n: u32, m: u32) -> u32 {
    let mut acc = 0;
    let mut i = 0;
    while i < n {
        let mut j = 0;
        while j < m { acc ^= j; j += 1; }
        i += 1;
    }
    acc
}
"#,
    );
    assert_eq!(saying(&out, UNDECLARED), Vec::<&str>::new(), "{out}");
}

/// The near misses of a counted `while` are refused, each naming its loop's line: a body that
/// can take the counter back, one that writes the limit, a step by a variable, a path round
/// the loop with no step, and `loop {}`.
#[test]
fn the_near_misses_of_a_counted_while_are_refused_naming_the_loop() {
    let out = may_of(
        "",
        r#"
#[blaze::may()]
pub fn falls(n: u32) -> u32 {
    let mut i = 0;
    while i < n { if i == 3 { i -= 1; } i += 1; }
    i
}
#[blaze::may()]
pub fn moves(mut n: u32) -> u32 {
    let mut i = 0;
    while i < n { n += 1; i += 1; }
    i
}
#[blaze::may()]
pub fn strides(n: u32, by: u32) -> u32 {
    let mut i = 0;
    while i < n { i += by; }
    i
}
#[blaze::may()]
pub fn skips(n: u32) -> u32 {
    let mut i = 0;
    while i < n { if i == 3 { continue; } i += 1; }
    i
}
#[blaze::may()]
pub fn spins() {
    loop {}
}
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 5, "{out}");
    for (name, line) in [("falls", 4), ("moves", 10), ("strides", 16), ("skips", 22), ("spins", 27)] {
        let finding = found.iter().find(|it| it.contains(&format!("`fn {name}` {DIVERGE}")));
        assert!(finding.is_some(), "{name}:\n{out}");
        assert!(out.contains(&format!("lib.rs:{line}")), "the loop of {name}, at line {line}:\n{out}");
    }
}

/// Whether a step can wrap is the profile's: where the crate keeps overflow checks a wrap is a
/// panic, which ends the function, so `i <= n` and a step by three end; where it drops them
/// `i` can wrap round and both loops are refused.
#[test]
fn a_step_that_can_wrap_bounds_nothing() {
    const LOOPS: &str = r#"
#[blaze::may(panic)]
pub fn upto(n: u32) -> u32 {
    let mut i = 0;
    while i <= n { i += 1; }
    i
}
#[blaze::may(panic)]
pub fn by_three(n: u32) -> u32 {
    let mut i = 0;
    while i < n { i += 3; }
    i
}
"#;
    let checked = may_of(" cfg:overflow_checks", LOOPS);
    assert_eq!(saying(&checked, UNDECLARED), Vec::<&str>::new(), "{checked}");
    let unchecked = may_of("", LOOPS);
    let found = saying(&unchecked, UNDECLARED);
    assert_eq!(found.len(), 2, "{unchecked}");
    assert!(found.iter().all(|it| it.contains(DIVERGE)), "{unchecked}");
}

/// A `while let` over a stack is not a loop these rules bound: refused until a `decreases`
/// clause can say why it ends.
#[test]
fn a_while_let_over_a_stack_is_refused() {
    let out = may_of(
        "",
        r#"
pub struct Stack { items: [u32; 4], len: usize }
impl Stack {
    pub fn pop(&mut self) -> Option<u32> {
        if self.len == 0 { return None; }
        self.len -= 1;
        Some(self.items[self.len])
    }
}
#[blaze::may(panic)]
pub fn drain(stack: &mut Stack) -> u32 {
    let mut acc = 0;
    while let Some(x) = stack.pop() { acc ^= x; }
    acc
}
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains(&format!("`fn drain` {DIVERGE}")), "{out}");
    assert!(out.contains("lib.rs:12"), "the loop's line:\n{out}");
}

// `diverge`: recursion and calls the check can't follow.

/// A function that calls itself, and two that call each other, are refused, naming the chain
/// and that the function is called again.
#[test]
fn recursion_is_refused_naming_the_chain() {
    let out = may_of(
        "",
        r#"
#[blaze::may()]
pub fn fact(n: u32) -> u32 { if n == 0 { 1 } else { fact(n - 1) } }
#[blaze::may()]
pub fn even(n: u32) -> bool { if n == 0 { true } else { odd(n - 1) } }
fn odd(n: u32) -> bool { if n == 0 { false } else { even(n - 1) } }
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 2, "{out}");
    let fact = found.iter().find(|it| it.contains("`fn fact`")).expect("fact's finding");
    assert!(fact.contains(DIVERGE) && fact.contains("it may call itself again"), "{out}");
    assert!(fact.contains("calls `main::fact`, which is called again from here"), "{out}");
    let even = found.iter().find(|it| it.contains("`fn even`")).expect("even's finding");
    assert!(even.contains("`main::even` → `main::odd`"), "the chain:\n{out}");
    assert!(even.contains("calls `main::even`, which is called again from here"), "{out}");
}

/// A callee whose list is `trusted` is taken at its word: its loop is not its caller's.
#[test]
fn a_trusted_list_is_taken_at_its_word() {
    let out = may_of(
        "",
        r#"
#[blaze::may(trusted = "counts down to zero")]
fn walk(n: u32) -> u32 {
    let mut i = n;
    loop { if i == 0 { return 0; } i -= 1; }
}
#[blaze::may()]
pub fn top(n: u32) -> u32 { walk(n) }
"#,
    );
    assert_eq!(saying(&out, UNDECLARED), Vec::<&str>::new(), "{out}");
}

/// A function pointer has no marks: one taken as a parameter may do anything, refused for
/// every word the list leaves out, naming the parameter; a call through one made in the body
/// may come back (`diverge`), naming the call, while what it points to was walked where it was
/// made and adds nothing else.
#[test]
fn a_call_through_a_function_pointer_is_refused_naming_it() {
    let out = may_of(
        "",
        r#"
#[blaze::may()]
pub fn apply(f: fn(u32) -> u32, x: u32) -> u32 { f(x) }
fn flip(x: u32) -> u32 { x ^ 1 }
#[blaze::may()]
pub fn inside(x: u32) -> u32 {
    let f: fn(u32) -> u32 = flip;
    f(x)
}
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 2, "{out}");
    let apply = found.iter().find(|it| it.contains("`fn apply`")).expect("apply's finding");
    assert!(apply.contains("takes the function pointer"), "{out}");
    assert!(apply.contains("may panic") && apply.contains(DIVERGE), "every word:\n{out}");
    let inside = found.iter().find(|it| it.contains("`fn inside`")).expect("inside's finding");
    assert!(inside.contains(&format!("`fn inside` {DIVERGE}")), "only `diverge`:\n{out}");
    assert!(inside.contains("function pointer"), "{out}");
    assert!(out.contains("lib.rs:7"), "the call's line:\n{out}");
}

/// A call through `dyn Trait` takes the trait method's list: a `dyn` of a trait whose methods
/// are listed passes, one of a trait with no list may do anything and is refused.
#[test]
fn a_call_through_dyn_takes_the_traits_marks() {
    let out = may_of(
        "",
        r#"
pub trait Job {
    #[blaze::may()]
    fn run(&self) -> u32;
}
pub trait Loose {
    fn run(&self) -> u32;
}
#[blaze::may()]
pub fn bounded(job: &dyn Job) -> u32 { job.run() }
#[blaze::may()]
pub fn loose(job: &dyn Loose) -> u32 { job.run() }
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`fn loose`") && found[0].contains("takes `dyn Loose"), "{out}");
}

// Traits, defaults, over-declaration, the old spellings.

/// A trait method's list is the bound of every impl: an impl with no list of its own is held
/// to it, one that lists fewer is fine, one that lists more is refused.
#[test]
fn an_impl_stays_within_its_trait_methods_list() {
    let out = may_of(
        "",
        r#"
pub trait Sink {
    #[blaze::may(panic)]
    fn put(&self, x: u32) -> u32;
}
pub struct Fewer;
impl Sink for Fewer {
    #[blaze::may()]
    fn put(&self, x: u32) -> u32 { x }
}
pub struct More;
impl Sink for More {
    #[blaze::may(panic, alloc)]
    fn put(&self, x: u32) -> u32 { x }
}
pub struct Plain;
impl Sink for Plain {
    fn put(&self, x: u32) -> u32 { let _ = alloc::vec::Vec::<u32>::with_capacity(4); x }
}
"#,
    );
    let more = saying(&out, "never more");
    assert_eq!(more.len(), 1, "{out}");
    assert!(more[0].contains("lists `alloc`") && more[0].contains("`#[blaze::may(panic)]`"), "{out}");
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("may allocate from the global heap"), "{out}");
    assert!(found[0].contains("the `#[blaze::may(panic)]` of the trait's method it implements"), "{out}");
    assert!(out.contains("lib.rs:17"), "`Plain`'s `put`:\n{out}");
}

/// A module's `#![blaze::may(..)]` is the default of the functions in it; the nearest one
/// holds; a function's own list replaces it entirely, nothing merged.
#[test]
fn a_modules_default_applies_and_a_functions_own_list_replaces_it() {
    let out = may_of(
        "",
        r#"
#![blaze::may(panic)]
pub fn pick(v: &[u32], i: usize) -> u32 { v[i] }
pub fn grow() { let _ = alloc::vec::Vec::<u32>::with_capacity(4); }
#[blaze::may(alloc, diverge)]
pub fn own(v: &[u32], i: usize) -> u32 { v[i] }
pub mod inner {
    #![blaze::may(panic, alloc, diverge)]
    pub fn grow() { let _ = alloc::vec::Vec::<u32>::with_capacity(4); }
}
"#,
    );
    let found = saying(&out, UNDECLARED);
    assert_eq!(found.len(), 2, "{out}");
    let grow = found.iter().find(|it| it.contains("`fn grow`")).expect("the outer grow's finding");
    assert!(grow.contains("may allocate from the global heap"), "{out}");
    assert!(grow.contains("the default of a module around it, `#![blaze::may(panic)]`"), "{out}");
    let own = found.iter().find(|it| it.contains("`fn own`")).expect("own's finding");
    assert!(own.contains("`fn own` may panic, which its `#[blaze::may(alloc, diverge)]`"), "{out}");
}

/// A word a private function lists and is proved not to need is advice to take it off; a
/// `pub` function and a trait's method may keep the right.
#[test]
fn an_unneeded_word_is_linted_on_a_private_function_only() {
    let out = may_of(
        " cfg:overflow_checks",
        r#"
#[blaze::may(panic, alloc)]
fn private(x: u32) -> u32 { x }
#[blaze::may(panic, alloc)]
pub fn public(x: u32) -> u32 { private(x) }
pub trait Shape {
    #[blaze::may(alloc)]
    fn sides(&self) -> u32 { 4 }
}
"#,
    );
    let found = saying(&out, UNNEEDED);
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`fn private` lists `panic`, `alloc`"), "{out}");
    assert_eq!(saying(&out, UNDECLARED), Vec::<&str>::new(), "{out}");
}

/// The old spellings still mean what they meant (their own lints hold them), and where
/// effects are declared the rewrite is the list they stand for: every word they do not take
/// off that the function is not proved free of.
#[test]
fn an_old_spelling_still_holds_and_is_rewritten_to_its_list() {
    let out = may_of(
        " cfg:overflow_checks",
        r#"
#![blaze::effects]
#[blaze::nopanic]
#[blaze::nonblocking]
pub fn old() { let _ = alloc::vec::Vec::<u32>::with_capacity(4); }
#[blaze::nonblocking]
pub fn locks(m: &std::sync::Mutex<u32>) { let _guard = m.lock(); }
"#,
    );
    let found = saying(&out, NEGATIVE);
    assert_eq!(found.len(), 2, "{out}");
    let old = found.iter().find(|it| it.contains("`fn old`")).expect("old's finding");
    assert!(old.contains("`#[blaze::nopanic]`, `#[blaze::nonblocking]`"), "{out}");
    assert!(old.contains("`#[blaze::may(alloc, diverge)]`"), "the list they stand for:\n{out}");
    // The old spelling is still checked by its own lint, and by nothing new.
    assert_eq!(saying(&out, "is `#[blaze::nonblocking]`, but may block the thread").len(), 1, "{out}");
    assert_eq!(saying(&out, UNDECLARED), Vec::<&str>::new(), "{out}");
}
