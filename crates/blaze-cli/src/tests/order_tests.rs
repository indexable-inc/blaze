//! A check's answer does not depend on how many workers ran it or the order its jobs were made
//! in (parallel.md, point 8; rule 25). A child of main.rs's tests: its helpers are theirs.
//!
//! The oracle: every record the command answered (its name, out, and digests of its payload,
//! findings and read list, work.rs `answers_for_test`), sorted by name, is equal for 1 worker,
//! 8 workers, and 8 workers with every round's jobs made in reverse order (so each file job
//! checks its bodies in reverse, over the one world it shares: work.rs `run_batches`). In the
//! simulation the pool runs each task inline (bstd task.rs, `Layout::Inline`): the worker count
//! changes how many batches a round holds, so which bodies share a file job and its world.
//!
//! The test also prints each kind's tasks made and entered (the profile's rows): entries equal
//! to made is a kind with no re-entry (parallel.md, point 1).

use super::*;
use crate::work::answers_for_test::{ANSWERED, Answer, CHECKED, REVERSED};

std::thread_local! {
    /// The files the last [`workspace`] wrote: what a differing record's item is quoted from.
    static WRITTEN: core::cell::RefCell<std::vec::Vec<(std::string::String, std::string::String)>> = const { core::cell::RefCell::new(std::vec::Vec::new()) };
}

/// Two members, `x` and `y` on `x`: a trait with a provided method and impls, generics with
/// bounds, an `impl Trait` return (an opaque whose hidden type another body reads), closures,
/// inherent methods, consts, a cross-crate call, and two type errors (E0308), spread over
/// files so a file job holds several bodies.
fn workspace(io: &Io, edited: bool) {
    WRITTEN.with(|w| w.borrow_mut().clear());
    let write = |io: &Io, path: &str, text: &str| {
        WRITTEN.with(|w| w.borrow_mut().push((std::string::String::from(path), std::string::String::from(text))));
        write(io, path, text);
    };
    write(io,"Cargo.toml", "[workspace]\nmembers = [\"x\", \"y\"]\nresolver = \"2\"\n");
    write(io, "x/Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "y/Cargo.toml", "[package]\nname = \"y\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nx = { path = \"../x\" }\n");
    write(io, "x/src/lib.rs", "pub mod shapes;\npub mod ops;\n\npub const BASE: u32 = 7;\n\npub fn base() -> u32 {\n    BASE + 1\n}\n");
    write(
        io,
        "x/src/shapes.rs",
        "pub trait Area {\n    fn area(&self) -> u32;\n    fn twice(&self) -> u32 {\n        self.area() * 2\n    }\n}\n\npub struct Square {\n    pub side: u32,\n}\n\npub struct Rect {\n    pub w: u32,\n    pub h: u32,\n}\n\nimpl Area for Square {\n    fn area(&self) -> u32 {\n        self.side * self.side\n    }\n}\n\nimpl Area for Rect {\n    fn area(&self) -> u32 {\n        self.w * self.h\n    }\n}\n\nimpl Rect {\n    pub fn new(w: u32, h: u32) -> Rect {\n        Rect { w, h }\n    }\n\n    pub fn square(&self) -> bool {\n        self.w == self.h\n    }\n}\n\npub fn total<T: Area>(items: &[T]) -> u32 {\n    let mut sum = 0;\n    for item in items {\n        sum += item.area();\n    }\n    sum\n}\n\npub fn wrong_area() -> u32 {\n    let _bad: u32 = true;\n    3\n}\n",
    );
    let ops = if edited {
        "pub fn add(a: u32, b: u32) -> u32 {\n    a + b + 1\n}\n"
    } else {
        "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n"
    };
    write(
        io,
        "x/src/ops.rs",
        &std::format!(
            "{ops}\npub fn apply<F: Fn(u32) -> u32>(f: F, v: u32) -> u32 {{\n    f(v)\n}}\n\npub fn adder(n: u32) -> impl Fn(u32) -> u32 {{\n    move |v| v + n\n}}\n\npub fn use_adder() -> u32 {{\n    let f = adder(2);\n    f(3)\n}}\n\npub fn pair(a: u32) -> (u32, u32) {{\n    (a, add(a, 1))\n}}\n\npub fn first(p: (u32, u32)) -> u32 {{\n    p.0\n}}\n\npub fn pick(flag: bool, a: u32, b: u32) -> u32 {{\n    if flag {{ a }} else {{ b }}\n}}\n"
        ),
    );
    write(
        io,
        "y/src/lib.rs",
        "use x::shapes::{Area, Rect, Square};\n\npub fn rects() -> u32 {\n    let r = Rect::new(2, 3);\n    r.area() + r.twice()\n}\n\npub fn squares() -> u32 {\n    let s = [Square { side: 1 }, Square { side: 2 }];\n    x::shapes::total(&s)\n}\n\npub fn closures() -> u32 {\n    let k = 4;\n    x::ops::apply(|v| v * k, 5)\n}\n\npub fn opaque() -> u32 {\n    let f = x::ops::adder(9);\n    f(1) + x::ops::use_adder()\n}\n\npub fn mismatch() -> u32 {\n    let _wrong: bool = 1u32;\n    x::base()\n}\n\npub fn pairs() -> u32 {\n    x::ops::first(x::ops::pair(x::ops::pick(true, 1, 2)))\n}\n",
    );
}

/// What one check answered, and the bodies and signatures checked in the order checked.
struct Run {
    answers: std::vec::Vec<Answer>,
    checked: std::vec::Vec<(blaze_drive::Name, std::string::String, u32)>,
    seen: Checked,
}

/// The workspace checked in a fresh world with `workers`, its jobs made in reverse when
/// `reversed`.
fn run(workers: u32, reversed: bool, edited: bool) -> Run {
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    workspace(&io, edited);
    ANSWERED.with(|a| a.borrow_mut().clear());
    CHECKED.with(|c| c.borrow_mut().clear());
    WORKERS.with(|w| w.set(workers));
    REVERSED.with(|r| r.set(reversed));
    let seen = check(&io, work::Sizes::USUAL);
    WORKERS.with(|w| w.set(1));
    REVERSED.with(|r| r.set(false));
    let mut answers = ANSWERED.with(|a| core::mem::take(&mut *a.borrow_mut()));
    answers.sort_unstable();
    let checked = CHECKED.with(|c| core::mem::take(&mut *c.borrow_mut()));
    Run { answers, checked, seen }
}

/// The records `a` and `b` answered differently: each named by its kind and, where a file job
/// checked it, its file and the first line of its item's text; with what differs (out, payload,
/// findings, reads).
fn differing(a: &Run, b: &Run) -> std::vec::Vec<std::string::String> {
    let mut said = std::vec::Vec::new();
    for row in a.answers.iter() {
        let Some(other) = b.answers.iter().find(|o| o.0 == row.0) else {
            said.push(std::format!("{:?} answered only once", row.0));
            continue;
        };
        if other == row {
            continue;
        }
        let what = [(row.1 != other.1, "out"), (row.2 != other.2, "payload"), (row.3 != other.3, "findings"), (row.4 != other.4, "reads")]
            .iter()
            .filter(|(moved, _)| *moved)
            .map(|(_, what)| *what)
            .collect::<std::vec::Vec<_>>()
            .join("+");
        let item = a.checked.iter().find(|(name, _, _)| *name == row.0).map(|(_, file, place)| {
            let files = WRITTEN.with(|w| w.borrow().clone());
            let text = files.iter().find(|(path, _)| file.ends_with(path.as_str())).map(|(_, text)| text.clone()).unwrap_or_default();
            let line = text.get(*place as usize..).and_then(|rest| rest.lines().next()).unwrap_or("").to_owned();
            std::format!("{file}: {line}")
        });
        said.push(std::format!("{:?} {} ({what})", row.0.kind, item.unwrap_or_default()));
    }
    said
}

/// The profile row of `kind`: (made, entries).
fn made_entries(profile: &str, kind: &str) -> (u32, u32) {
    let row = profile.lines().find(|line| line.split_whitespace().next() == Some(kind)).unwrap_or("");
    let words: std::vec::Vec<&str> = row.split_whitespace().collect();
    let after = |label: &str| words.iter().position(|word| *word == label).and_then(|at| words.get(at + 1)).and_then(|n| n.parse().ok()).unwrap_or(0);
    (after("made"), after("entries"))
}

/// Each kind's tasks made and entered in `r`: `BodyCheck made M entries E; ...`.
fn made_line(r: &Run) -> std::string::String {
    let mut line = std::string::String::new();
    for kind in ["BodyCheck", "Signature", "CrateScope", "FileItems"] {
        let (made, entries) = made_entries(&r.seen.profile, kind);
        line.push_str(&std::format!("{kind} made {made} entries {entries}; "));
    }
    line
}

/// Both runs clean, with the fixture's two type errors.
fn clean(label: &str, r: &Run) {
    assert!(r.seen.failed.is_empty(), "{label}: {:?}\n{}", r.seen.failed, r.seen.profile);
    assert_eq!(r.seen.errors, 2, "{label}: the two type errors, once each: {:?}\n{}", r.seen.words, r.seen.profile);
}

/// Expected line: `test tests::order_tests::one_worker_and_eight_answer_alike ... ok`.
#[test]
fn one_worker_and_eight_answer_alike() {
    let one = run(1, false, false);
    let eight = run(8, false, false);
    clean("1 worker", &one);
    clean("8 workers", &eight);
    assert!(one.seen.bodies >= 20, "only {} bodies checked:\n{}", one.seen.bodies, one.seen.profile);
    assert!(one.answers.len() >= 40, "only {} records answered:\n{}", one.answers.len(), one.seen.profile);
    let moved = differing(&one, &eight);
    assert!(moved.is_empty(), "ORACLE: 8 workers answered other records than 1: {moved:?}\n{}", made_line(&one));
    assert_eq!(one.answers.len(), eight.answers.len());
}

/// The same with every round's jobs made in reverse, so a file job checks its bodies in reverse
/// over its one world. FAILS on the old world (run 2026-11-17 09:27, ask 092737-PAR-f67800):
/// the `Signature` outs of `impl Area for Rect`'s `area` and `impl Rect`'s `new` and `square`
/// moved with the order, and the reads of the four bodies that read them moved with them. The
/// cause (lane FH): the entry's want (head_digest.rs `want_at_entry`) asked the key map for the
/// impl fn's FILED member key, which exists only once the impl's head was made in the job's
/// world: made before, the out held the head's digest; not yet, it held the tokens alone. The
/// key is computed from the container and the name now.
#[test]
fn reversed_order_answers_alike() {
    let one = run(1, false, false);
    let reversed = run(8, true, false);
    clean("1 worker", &one);
    clean("8 reversed", &reversed);
    // The cause is applied: the reversed run checked the same bodies in another order.
    let eight = run(8, false, false);
    let (mut forward, mut backward) = (eight.checked.clone(), reversed.checked.clone());
    assert!(!forward.is_empty(), "no body was checked on the test's thread");
    assert_ne!(eight.checked, reversed.checked, "the reversed run checked its bodies in the same order");
    forward.sort_unstable();
    backward.sort_unstable();
    assert_eq!(forward, backward, "the reversed run checked other bodies");
    let moved = differing(&one, &reversed);
    assert!(moved.is_empty(), "ORACLE: the reversed order answered other records: {moved:?}\n{}", made_line(&one));
}

/// Control: the oracle sees a change. One body's text edited (`add` returns one more), the
/// answers differ.
#[test]
fn the_order_oracle_sees_an_edited_body() {
    let plain = run(1, false, false);
    let edited = run(1, false, true);
    assert!(plain.seen.failed.is_empty() && edited.seen.failed.is_empty());
    assert_ne!(plain.answers, edited.answers, "ORACLE BLIND: an edited body answered the same records");
}
